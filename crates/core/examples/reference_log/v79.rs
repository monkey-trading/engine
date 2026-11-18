//! The session's `V79` part: take-profit and stop-loss levels on a position. A vote activates the
//! version, which gives BTC its close-spread ceiling as its minimum level distance. A buy names both
//! levels, signed under `OrderWithLevels`, and its long takes them; a request moving the stop loss inside
//! the distance is refused, and one moving it out of reach sets it. A resting buy limit names its own
//! levels; a cancel-all leaves the long's levels and the limit's go with it. A price gaps through the
//! stop loss, which fills the whole long at its closing price and removes the take profit. A second long
//! takes its levels from a resting limit's fill, adds and keeps them, and a price lifts its closing price
//! past the take profit, which fills at its level or better. A passed vote moves BTC's distance within
//! its launch value to twice it, and one past twice it is refused.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::governance::VotedChange;
use engine_core::levels::{LevelKind, LevelsRemoved, PositionLevels};
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::batch::cancel_all;
use crate::common::close_spread::{attest, fund, trade};
use crate::common::events::only_fill;
use crate::common::levels::{closing_price, order_with, removed, set, set_event, triggered};
use crate::common::{BTC, Client, Harness};
use crate::v56::pass;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x79;

/// Activates `V79` by vote; then the levels described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V79 };
    let events = pass(h, 170, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 170, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V79);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);
    let ceiling = h.engine.close_spread_ceilings().and_then(|ceilings| ceilings.get(&BTC)).copied();
    let distance = h.engine.markets()[&BTC].spec.min_level_distance_bps;
    assert!(distance.is_some() && distance == ceiling, "{distance:?} {ceiling:?}");
    let start = h.engine.markets()[&BTC].mark.expect("BTC has traded");
    h.price(BTC, &start.to_string());

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "1000000");

    // A buy naming both levels, 2% either side, and a stop loss moved inside the distance and then out.
    let named = PositionLevels { take_profit: Some(away(start, 200)), stop_loss: Some(away(start, -200)) };
    let events = order_with(h, &client, BTC, Side::Buy, "0.1", market(start), false, named);
    assert_eq!(set_event(&events), named);
    let near = closing_price(h, &client, BTC) - Fixed::ONE;
    let events = set(h, &client, BTC, PositionLevels { stop_loss: Some(near), ..named });
    assert!(
        matches!(events.as_slice(), [Event::OrderRejected { reason: Reject::LevelTooClose { .. }, .. }]),
        "{events:?}"
    );
    let moved = PositionLevels { stop_loss: Some(away(start, -300)), ..named };
    assert_eq!(set_event(&set(h, &client, BTC, moved)), moved);

    // A resting limit naming its own levels; a cancel-all takes it and leaves the long's.
    let limit = OrderKind::Limit { price: away(start, -500) };
    let below = PositionLevels { take_profit: Some(start), stop_loss: Some(away(start, -1_000)) };
    order_with(h, &client, BTC, Side::Buy, "0.1", limit, false, below);
    let nonce = h.engine.next_request_nonce(client.account);
    let events = h.cmd(cancel_all(&client, client.account, nonce));
    assert!(matches!(events.as_slice(), [Event::OrderCancelled { .. }]), "{events:?}");
    assert_eq!(h.engine.accounts()[&client.account].levels.get(&BTC), Some(&moved));

    // A price gapping through the stop loss fills the whole long at its closing price.
    let events = h.price(BTC, &away(start, -400).to_string());
    assert_eq!(triggered(&events).map(|(kind, _)| kind), Some(LevelKind::StopLoss));
    assert!(only_fill(&events).price < away(start, -300), "{events:?}");
    assert_eq!(removed(&events), LevelsRemoved::StopLoss);

    // A resting limit's fill gives the long it opens the limit's levels; an add keeps them, and a price
    // lifting the closing price past the take profit fills it at its level or better.
    let low = away(start, -400);
    let entry = PositionLevels { take_profit: Some(away(low, 100)), stop_loss: Some(away(low, -300)) };
    order_with(h, &client, BTC, Side::Buy, "0.1", OrderKind::Limit { price: away(low, 10) }, false, entry);
    assert_eq!(h.engine.accounts()[&client.account].levels.get(&BTC), Some(&entry));
    only_fill(&trade(h, &client, BTC, Side::Buy, "0.1", &low.to_string()));
    assert_eq!(h.engine.accounts()[&client.account].levels.get(&BTC), Some(&entry));
    let events = h.price(BTC, &away(low, 120).to_string());
    assert_eq!(triggered(&events).map(|(kind, _)| kind), Some(LevelKind::TakeProfit));
    let fill = only_fill(&events);
    assert!(fill.price >= away(low, 100) && fill.qty == Fixed::from_raw(200_000_000), "{fill:?}");

    // A vote moves BTC's distance up to twice its launch value, and one past it is refused.
    let launch = distance.expect("checked above");
    let relisting =
        |bps: i64| MarketSpec { min_level_distance_bps: Some(bps), ..h.engine.markets()[&BTC].spec.clone() };
    let (wider, too_wide) = (relisting(2 * launch), relisting(2 * launch + 1));
    let (raise, refuse) = (AdminCommand::UpsertMarket(wider), AdminCommand::UpsertMarket(too_wide));
    for (proposal, command) in [(171, &refuse), (172, &raise)] {
        let events = pass(h, proposal, false, Some(command.clone()));
        assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    }
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 171, command: refuse })));
    assert!(matches!(events.last(), Some(Event::AdminRejected { .. })), "{events:?}");
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 172, command: raise })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.markets()[&BTC].spec.min_level_distance_bps, Some(2 * launch));
}

/// `price` moved `bps` either way.
fn away(price: Fixed, bps: i64) -> Fixed {
    price + price.mul_bps(bps)
}

/// A market order quoting `price`.
fn market(price: Fixed) -> OrderKind {
    OrderKind::Market { quoted: price, max_slippage_bps: None }
}
