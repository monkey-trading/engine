//! The session's `V52` part: every fill pays its size spread, in tenths of a bp, and a listed
//! market's bands move only after a listing notice, within its launch bands.

use engine_core::access::AccessDeposit;
use engine_core::command::{AdminCommand, Command};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::{MarketSpec, Tier};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::spread_tier::SpreadTier;
use engine_core::types::{AssetClass, MarketId, Side};

use crate::common::close_spread::{attest, evidence, fund, reduce, trade};
use crate::common::{BTC, Client, Harness, f, market};
use crate::v51::MEMBER_SEEDS;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x62;
/// The market this part lists through a bound ceiling adoption.
const LISTED: MarketId = 52;

fn sign(h: &mut Harness, command: AdminCommand) -> Vec<Event> {
    h.operate_as(MEMBER_SEEDS[0], command)
}

fn refused(events: &[Event]) {
    assert!(matches!(events.last(), Some(Event::AdminRejected { .. })), "{events:?}");
}

fn applied(events: &[Event]) {
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
}

/// `V52` activates over the listed markets, writing their bands in tenths of a bp at their floors. A
/// client's open and close of BTC each pay the bands. A relisting below the floor or without a notice
/// is refused, as is a notice above twice the launch rates; a notice of doubled bands and a raised
/// margin applies at its time, and a market listed through an adoption that bound its listing prices.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V52 });
    let btc = h.engine.markets()[&BTC].spec.clone();
    assert!(btc.spread_tiers.iter().all(|tier| !tier.is_whole_bps() && tier.tenth_bps() >= 10), "{btc:?}");

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "1000000");
    let house = AccessDeposit {
        account: None,
        payer: Some(client.account),
        amount: f("2000000"),
        chain_ref: "0xhouse:52".into(),
    };
    let ev = evidence(h, eip712::deposit_screening_hash(&house), None, Some(client.account));
    h.cmd(Command::AccessDeposit { deposit: house, evidence: ev });
    h.price(BTC, "80000");
    let open = only_fill(&trade(h, &client, BTC, Side::Buy, "2", "80000"));
    let close = only_fill(&reduce(h, &client, BTC, Side::Sell, "2", "80000"));
    assert!(open.price > f("80000") && close.price < f("80000"), "{open:?} {close:?}");

    let scaled = |by: i64| -> Vec<SpreadTier> {
        btc.spread_tiers.iter().map(|tier| SpreadTier::tenths(tier.up_to, tier.tenth_bps() * by)).collect()
    };
    let below_floor: Vec<SpreadTier> =
        btc.spread_tiers.iter().map(|tier| SpreadTier::tenths(tier.up_to, 9.min(tier.tenth_bps()))).collect();
    refused(&sign(h, AdminCommand::UpsertMarket(MarketSpec { spread_tiers: below_floor, ..btc.clone() })));
    let doubled = MarketSpec {
        spread_tiers: scaled(2),
        margin_tiers: btc.margin_tiers.iter().map(|tier| Tier { bps: tier.bps + 100, ..*tier }).collect(),
        ..btc.clone()
    };
    refused(&sign(h, AdminCommand::UpsertMarket(doubled.clone())));

    let due = h.ts + NOTICE_MS;
    let notice = |spread_tiers| ListingNotice {
        spread_tiers: Some(spread_tiers),
        margin_tiers: Some(doubled.margin_tiers.clone()),
        markup_bps: None,
        effective_at: due,
    };
    refused(&sign(h, AdminCommand::AnnounceListing { market: BTC, notice: notice(scaled(3)) }));
    applied(&sign(h, AdminCommand::AnnounceListing { market: BTC, notice: notice(scaled(2)) }));
    let listing = MarketSpec {
        spread_tiers: vec![SpreadTier::tenths(None, 30)],
        ..market(LISTED, "LINK-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)
    };
    let adoption = |listing| AdminCommand::AdoptCloseSpreadCeiling {
        market: LISTED,
        max_close_spread_bps: 500,
        effective_at: due,
        listing,
    };
    refused(&sign(h, adoption(None)));
    applied(&sign(h, adoption(Some(listing.clone()))));
    h.advance_to(due - 1);
    refused(&sign(h, AdminCommand::UpsertMarket(doubled.clone())));
    h.advance_to(due);
    applied(&sign(h, AdminCommand::UpsertMarket(doubled)));
    refused(&sign(h, AdminCommand::UpsertMarket(MarketSpec { symbol: "LINK-PERP".into(), ..listing.clone() })));
    applied(&sign(h, AdminCommand::UpsertMarket(listing)));
    assert!(h.engine.operator_bounds().markups.is_empty() && h.engine.operator_bounds().proposed_listings.is_empty());
}

fn only_fill(events: &[Event]) -> engine_core::event::Fill {
    crate::common::events::only_fill(events).clone()
}
