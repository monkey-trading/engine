//! Markets priced from futures from `V21`: the index follows the front contract, records the gap to
//! the next one, rolls onto it on that gap, and refuses quotes for contracts it does not take, end
//! to end through `Engine::apply`.

mod common;

use chrono::{DateTime, Utc};
use common::events::{fills, rejection};
use common::{Harness, acct, f, market, ny, us_equities};
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::futures::FuturesContract;
use engine_core::market::MarketSpec;
use engine_core::price::{PriceError, SourceQuote};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Millis, Side};

/// US 500, priced from E-mini futures.
const US500: MarketId = 20;
/// How long after the harness start the index rolls from December onto March.
const ROLL_AFTER_MS: Millis = 3_600_000;

#[test]
fn the_front_and_next_contracts_are_taken_and_others_refused() {
    let mut h = listed();
    let ev = h.cmd(prices(&h, "ESZ6", &["6000", "6001"]));
    assert!(ev.iter().all(|e| !matches!(e, Event::PricesRejected { .. })), "{ev:?}");
    assert_eq!(index(&h), Ok(f("6000.5")));
    // March, next, is taken before its roll time and leaves the index on December.
    let ev = h.cmd(prices(&h, "ESH7", &["6040", "6041"]));
    assert!(ev.iter().all(|e| !matches!(e, Event::PricesRejected { .. } | Event::MarketRolled { .. })), "{ev:?}");
    assert_eq!(h.engine.markets()[&US500].quotes_for(Some("ESH7")).map(|q| q.len()), Some(2));
    assert_eq!(index(&h), Ok(f("6000.5")));

    // June, two ahead, and a quote naming no contract are refused.
    for contract in [Some("ESM7"), None] {
        let ev = h.cmd(Command::Prices { market: US500, quotes: vec![quote(&h, contract, "6080")] });
        assert!(refused(&ev), "{contract:?}: {ev:?}");
    }
    assert_eq!(index(&h), Ok(f("6000.5")));
}

#[test]
fn a_roll_takes_the_gap_so_the_index_and_positions_do_not_move() {
    let mut h = listed();
    let a = acct(1);
    h.deposit(a, "1000000");
    h.cmd(prices(&h, "ESZ6", &["6000", "6001"]));
    h.market_order(a, US500, Side::Buy, "1", "6000.5");
    // A buy stop 20 above the index, which a switch to March unadjusted would jump through.
    h.order(a, US500, Side::Buy, "1", OrderKind::Stop { price: f("6020") });

    h.advance_to(h.ts + ROLL_AFTER_MS);
    h.cmd(prices(&h, "ESZ6", &["6000", "6001"]));
    // March trades 40 above December: the roll takes the gap into the adjustment.
    let ev = h.cmd(prices(&h, "ESH7", &["6040", "6041"]));
    let rolled = Event::MarketRolled { market: US500, contract: "ESH7".into(), adjustment: f("-40") };
    assert!(ev.contains(&rolled), "{ev:?}");
    assert!(fills(&ev).is_empty(), "{ev:?}");
    assert_eq!(index(&h), Ok(f("6000.5")));
    assert_eq!(h.engine.markets()[&US500].mark, Some(f("6000.5")));
    assert_eq!(h.engine.accounts()[&a].orders.len(), 1);

    // From the roll, the index follows March. December's quotes, which feeds send until they learn
    // of the roll, are dropped without a rejection.
    h.advance(100);
    let ev = h.cmd(prices(&h, "ESZ6", &["6100", "6101"]));
    assert!(!refused(&ev) && index(&h) == Ok(f("6000.5")), "{ev:?}");
    let ev = h.cmd(prices(&h, "ESH7", &["6050", "6051"]));
    assert_eq!(index(&h), Ok(f("6010.5")));
    assert!(fills(&ev).is_empty(), "{ev:?}");
    let (equity, _) = h.equity_and_margin(a);
    assert_eq!(equity - h.balance(a), f("10"));
}

#[test]
fn a_roll_uses_the_last_recorded_gap_after_the_front_stops_quoting() {
    let mut h = listed();
    // Both contracts quote before the roll time, so the gap is recorded: December 40 below March.
    h.cmd(prices(&h, "ESZ6", &["6000", "6001"]));
    h.cmd(prices(&h, "ESH7", &["6040", "6041"]));
    assert_eq!(h.engine.markets()[&US500].futures.as_ref().and_then(|f| f.gap), Some(f("-40")));
    // December expires during an outage; after it only March quotes, 25 higher.
    h.advance_to(h.ts + ROLL_AFTER_MS + 86_400_000);
    let ev = h.cmd(prices(&h, "ESH7", &["6065", "6066"]));
    let rolled = Event::MarketRolled { market: US500, contract: "ESH7".into(), adjustment: f("-40") };
    assert!(ev.contains(&rolled), "{ev:?}");
    assert_eq!(index(&h), Ok(f("6025.5")));
}

#[test]
fn a_roll_waits_for_a_gap_and_an_index_on_the_next_contract() {
    let mut h = listed();
    h.advance_to(h.ts + ROLL_AFTER_MS);
    // March alone has never quoted beside December, so there is no gap to roll on.
    let ev = h.cmd(prices(&h, "ESH7", &["6040", "6041"]));
    assert!(!ev.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "{ev:?}");
    assert_eq!(index(&h), Err(PriceError::Stale { fresh: 0, needed: 2 }));
    // With March stale, December alone records no gap either.
    h.advance(2_000);
    let ev = h.cmd(prices(&h, "ESZ6", &["6000", "6001"]));
    assert!(!ev.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "{ev:?}");

    let ev = h.cmd(prices(&h, "ESH7", &["6040", "6041"]));
    assert!(ev.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "{ev:?}");
    assert_eq!(index(&h), Ok(f("6000.5")));
}

#[test]
fn an_adjusted_index_outside_the_input_range_is_no_price() {
    let mut h = listed();
    h.deposit(acct(1), "1000000");
    h.advance_to(h.ts + ROLL_AFTER_MS);
    h.cmd(prices(&h, "ESZ6", &["10", "10"]));
    h.cmd(prices(&h, "ESH7", &["1000", "1000"]));
    assert_eq!(index(&h), Ok(f("10")));
    // March halves: 500 less the 990 adjustment is below zero.
    h.advance(100);
    h.cmd(prices(&h, "ESH7", &["500", "500"]));
    assert_eq!(index(&h), Err(PriceError::OutOfRange { mid: f("-490") }));
    let ev = h.market_order(acct(1), US500, Side::Buy, "1", "10");
    assert_eq!(rejection(&ev), &Reject::NoPrice { error: PriceError::OutOfRange { mid: f("-490") } });
}

#[test]
fn a_relisting_keeps_the_roll_and_refuses_to_drop_the_front_contract() {
    let mut h = listed();
    h.advance_to(h.ts + ROLL_AFTER_MS);
    h.cmd(prices(&h, "ESZ6", &["6000", "6001"]));
    h.cmd(prices(&h, "ESH7", &["6040", "6041"]));
    let rolled = h.engine.markets()[&US500].futures.clone();

    // A listing that adds September keeps the market on March at its adjustment.
    let mut more = spec();
    more.contracts.push(contract("ESU7", "2027-06-10T13:30:00Z"));
    let ev = h.operate(AdminCommand::UpsertMarket(more));
    assert!(ev.contains(&Event::AdminApplied), "{ev:?}");
    assert_eq!(h.engine.markets()[&US500].futures, rolled);

    let refused = |h: &mut Harness, spec: MarketSpec, expected: &str| {
        let ev = h.operate(AdminCommand::UpsertMarket(spec));
        assert!(matches!(&ev[..], [Event::AdminRejected { reason }] if reason.contains(expected)), "{ev:?}");
    };
    let mut without_front = spec();
    without_front.contracts.retain(|c| c.contract != "ESH7");
    refused(&mut h, without_front, "ESH7");
    refused(&mut h, MarketSpec { contracts: vec![], ..spec() }, "prices from futures");
    let spot = market(21, "US100", AssetClass::Index, Schedule::AlwaysOpen, 500);
    h.operator_admin(AdminCommand::UpsertMarket(spot.clone()));
    refused(&mut h, MarketSpec { contracts: spec().contracts, ..spot }, "listed without futures");
}

#[test]
fn a_relisting_keeps_the_next_quotes_and_gap_only_while_the_next_contract_stays_next() {
    let mut h = listed();
    h.cmd(prices(&h, "ESZ6", &["6000", "6001"]));
    h.cmd(prices(&h, "ESH7", &["6040", "6041"]));
    h.operator_admin(AdminCommand::UpsertMarket(spec()));
    let futures = h.engine.markets()[&US500].futures.clone().expect("priced from futures");
    assert_eq!((futures.next_quotes.len(), futures.gap), (2, Some(f("-40"))));

    // With June listed between December and March, June is next: March's quotes and gap go.
    let mut june_next = spec();
    june_next.contracts.retain(|c| c.contract != "ESM7");
    june_next.contracts.insert(1, contract("ESM7", &rfc3339(h.ts + 60_000)));
    h.operator_admin(AdminCommand::UpsertMarket(june_next));
    let futures = h.engine.markets()[&US500].futures.as_ref().expect("priced from futures");
    assert!(futures.next_quotes.is_empty() && futures.gap.is_none(), "{futures:?}");
}

#[test]
fn a_close_drops_both_contracts_quotes_and_keeps_the_gap() {
    let mut h = Harness::v21();
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { schedule: us_equities(), ..spec() }));
    h.cmd(prices(&h, "ESZ6", &["6000", "6001"]));
    h.cmd(prices(&h, "ESH7", &["6040", "6041"]));
    h.advance_to(ny(2026, 10, 20, 16, 0));
    assert!(h.cmd(Command::Tick).contains(&Event::MarketClosed { market: US500 }));
    let market = &h.engine.markets()[&US500];
    let futures = market.futures.as_ref().expect("priced from futures");
    assert!(market.quotes.is_empty() && futures.next_quotes.is_empty(), "{market:?}");
    assert_eq!(futures.gap, Some(f("-40")));
}

#[test]
fn a_quote_naming_a_contract_is_refused_by_a_market_not_priced_from_futures() {
    let mut h = listed();
    let spot = market(21, "US100", AssetClass::Index, Schedule::AlwaysOpen, 500);
    h.operator_admin(AdminCommand::UpsertMarket(spot));
    let ev = h.cmd(Command::Prices { market: 21, quotes: vec![quote(&h, Some("NQZ6"), "21000")] });
    assert!(refused(&ev), "{ev:?}");
    assert!(h.engine.markets()[&21].quotes.is_empty());
    assert!(!h.engine.markets()[&21].names_contract(Some("NQZ6")));
    assert!(h.engine.markets()[&US500].names_contract(Some("ESM7")));
}

#[test]
fn a_listing_starts_on_the_last_contract_whose_roll_time_has_passed() {
    let mut h = listed();
    assert_eq!(h.engine.markets()[&US500].futures.as_ref().map(|f| f.contract.as_str()), Some("ESZ6"));
    let late = MarketSpec { id: 22, symbol: "US500-LATE".into(), ..spec() };
    h.advance_to(h.ts + ROLL_AFTER_MS);
    h.operator_admin(AdminCommand::UpsertMarket(late));
    let futures = h.engine.markets()[&22].futures.as_ref().expect("priced from futures");
    assert_eq!((futures.contract.as_str(), futures.adjustment), ("ESH7", f("0")));
}

#[test]
fn futures_contracts_need_v21_and_must_be_named_once_in_roll_order() {
    let mut h = Harness::v17();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
    let ev = h.operate(AdminCommand::UpsertMarket(spec()));
    assert!(matches!(&ev[..], [Event::AdminRejected { reason }] if reason.contains("V21")), "{ev:?}");

    let base = spec();
    let mut twice = base.clone();
    twice.contracts[1].contract = "ESZ6".into();
    let mut out_of_order = base.clone();
    out_of_order.contracts.swap(1, 2);
    let mut unnamed = base;
    unnamed.contracts[0].contract = String::new();
    for spec in [twice, out_of_order, unnamed] {
        assert!(spec.validate().is_err(), "{:?}", spec.contracts);
    }
}

// ── Helpers ──

/// `V21` with US 500 listed on December, rolling onto March `ROLL_AFTER_MS` after the start.
fn listed() -> Harness {
    let mut h = Harness::v21();
    h.operator_admin(AdminCommand::UpsertMarket(spec()));
    h
}

/// US 500 around the 24/7 clock, priced from December, March, and June E-minis.
fn spec() -> MarketSpec {
    let contracts = vec![
        contract("ESZ6", "2026-09-10T13:30:00Z"),
        contract("ESH7", &rfc3339(common::start() + ROLL_AFTER_MS)),
        contract("ESM7", "2027-03-11T13:30:00Z"),
    ];
    MarketSpec { contracts, ..market(US500, "US500", AssetClass::Index, Schedule::AlwaysOpen, 500) }
}

/// A contract rolling in at `from`, an RFC 3339 time.
fn contract(name: &str, from: &str) -> FuturesContract {
    FuturesContract { contract: name.into(), from: from.parse().expect("an RFC 3339 time") }
}

/// `ts` as RFC 3339.
fn rfc3339(ts: Millis) -> String {
    DateTime::<Utc>::from_timestamp_millis(ts).expect("in range").to_rfc3339()
}

/// One venue's quote at `mid` naming `contract`, stamped now.
fn quote(h: &Harness, contract: Option<&str>, mid: &str) -> SourceQuote {
    SourceQuote { contract: contract.map(Into::into), ..common::quote("venue0", mid, h.ts) }
}

/// One venue per mid quoting `contract`, stamped now.
fn prices(h: &Harness, contract: &str, mids: &[&str]) -> Command {
    let quotes = mids
        .iter()
        .enumerate()
        .map(|(i, mid)| SourceQuote {
            contract: Some(contract.into()),
            ..common::quote(&format!("venue{i}"), mid, h.ts)
        })
        .collect();
    Command::Prices { market: US500, quotes }
}

/// Whether `events` refuse a quote as one for a contract the market does not take.
fn refused(events: &[Event]) -> bool {
    events.iter().any(|e| matches!(e, Event::PricesRejected { reason, .. } if reason.contains("does not take")))
}

/// US 500's index mid now.
fn index(h: &Harness) -> Result<engine_core::Fixed, PriceError> {
    h.engine.markets()[&US500].index(h.ts).map(|index| index.mid)
}
