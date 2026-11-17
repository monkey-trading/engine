//! The session's `V74` part: a price re-checks only the resting limits it could fill. A vote activates
//! the version. BTC's least fill rate is its 2bp first band, under its 30bp ceiling. Below the day's
//! start, where the clients' long leaves the house ahead, a buy limit 1bp above the price rests; a price
//! at its limit passes it without pricing it, and one 4bp lower fills it within its limit. A reduce-only
//! sell limit rests 1bp above the price and its position closes; a price at its limit leaves it resting,
//! where an earlier version cancelled it, and one 4bp higher cancels it as unfillable. A sell limit rests
//! 1bp under a price 1% above the day's start: that price raises BTC to level 2 on the house's loss
//! without pricing the limit, and a price 4bp higher prices it at level 2, where it fails on its limit
//! and rests until the client cancels it once BTC is back at level 0.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{CancelReason, Event};
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{OrderId, Side};

use crate::common::close_spread::{attest, fund, reduce, rest, rest_as};
use crate::common::events::only_fill;
use crate::common::{BTC, Client, Harness};
use crate::v56::pass;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x74;
/// BTC's least fill rate here, its first band, in tenths of a bp.
const RATE: i128 = 20;
/// The stress cooldown in force.
const COOLDOWN_MS: i64 = 300_000;

/// Activates `V74` by vote; then the limits described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V74 };
    let events = pass(h, 160, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 160, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V74);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);
    let btc = &h.engine.markets()[&BTC];
    let least = btc.spec.spread_tiers.iter().map(|tier| i128::from(tier.tenth_bps())).min();
    let ceiling = h.engine.close_spread_ceilings().and_then(|ceilings| ceilings.get(&BTC)).copied();
    assert_eq!(least, Some(RATE));
    assert!(ceiling.is_some_and(|bps| i128::from(bps) * 10 > RATE), "{ceiling:?}");
    let start = btc.mark.expect("BTC has traded");

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "1000000");

    // A buy limit out of reach, passed by a price at its limit and filled by a price that reaches it.
    let base = start - Fixed::from_int(100);
    h.price(BTC, &base.to_string());
    let buy_at = base + share(base, RATE / 2);
    let buy = resting(&rest(h, &client, BTC, Side::Buy, "0.1", limit(buy_at)));
    untouched(&h.price(BTC, &buy_at.to_string()), buy);
    let low = buy_at - share(buy_at, 2 * RATE);
    let fill = only_fill(&h.price(BTC, &low.to_string())).clone();
    assert!(fill.order_id == buy && fill.price <= buy_at, "{fill:?}");

    // A reduce-only sell limit whose position closes rests past its limit until a price reaches it.
    let sell_at = low + share(low, RATE / 2);
    let reducing = resting(&rest_as(h, &client, BTC, Side::Sell, "0.1", limit(sell_at), true));
    only_fill(&reduce(h, &client, BTC, Side::Sell, "0.1", &low.to_string()));
    untouched(&h.price(BTC, &sell_at.to_string()), reducing);
    let events = h.price(BTC, &(sell_at + share(sell_at, 2 * RATE)).to_string());
    let mut cancelled = events.iter().filter_map(|event| match event {
        Event::OrderCancelled { order_id, reason, .. } => Some((*order_id, *reason)),
        _ => None,
    });
    assert!(cancelled.any(|cancel| cancel == (reducing, CancelReason::Unfillable)), "{events:?}");

    // A price 1% above the day's start raises BTC to level 2 and passes a sell limit it leaves unpriced;
    // the next price reaches the limit and prices it at level 2, short of its limit.
    let up = start + start.mul_bps(100);
    let sell_at = up - share(up, RATE / 2);
    let selling = resting(&rest(h, &client, BTC, Side::Sell, "0.1", limit(sell_at)));
    let events = h.price(BTC, &up.to_string());
    untouched(&events, selling);
    assert!(events.iter().any(|event| matches!(event, Event::StressChanged { market: BTC, level: 2 })), "{events:?}");
    untouched(&h.price(BTC, &(up + share(up, 2 * RATE)).to_string()), selling);
    assert!(h.engine.accounts()[&client.account].orders.contains_key(&selling));

    // Back at the day's start the house is ahead, and after the cooldown BTC steps down.
    h.price(BTC, &start.to_string());
    h.advance(COOLDOWN_MS);
    h.price(BTC, &start.to_string());
    assert_eq!(h.engine.markets()[&BTC].stress.level, 0);
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    let events = h.cmd(client.cancel(h.engine.next_request_nonce(client.account), selling));
    assert!(matches!(events.as_slice(), [Event::OrderCancelled { reason: CancelReason::Client, .. }]), "{events:?}");
}

/// `tenths` tenths of a bp of `price`, rounded down.
fn share(price: Fixed, tenths: i128) -> Fixed {
    Fixed::from_raw(price.raw() * tenths / 100_000)
}

/// A limit at `price`.
fn limit(price: Fixed) -> OrderKind {
    OrderKind::Limit { price }
}

/// The id of the order `events` accepted, which nothing filled or cancelled.
fn resting(events: &[Event]) -> OrderId {
    let id = events.iter().find_map(|event| match event {
        Event::OrderAccepted { order_id, .. } => Some(*order_id),
        _ => None,
    });
    let id = id.unwrap_or_else(|| panic!("accepted: {events:?}"));
    untouched(events, id);
    id
}

/// Asserts that `events` neither fill nor cancel order `id`.
fn untouched(events: &[Event], id: OrderId) {
    let touched = events.iter().any(|event| match event {
        Event::Fill(fill) => fill.order_id == id,
        Event::OrderCancelled { order_id, .. } => *order_id == id,
        _ => false,
    });
    assert!(!touched, "order {id}: {events:?}");
}
