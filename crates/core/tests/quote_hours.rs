//! Quotes and venue hours from `V21`: a quote stamped while its market is closed is refused, and a
//! close drops the market's quotes, so no quote from outside a market's hours ever fills an order,
//! end to end through `Engine::apply`.

mod common;

use chrono::NaiveDateTime;
use common::events::{fills, rejection};
use common::{AAPL, Harness, acct, f, market, ny, quote, us_equities};
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::market::{MarketSpec, PriceRules};
use engine_core::price::PriceError;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::{Period, Schedule};
use engine_core::types::{AccountId, AssetClass, MarketId, Side};

/// A share listed with an hour-long quote window and a one-minute closure at 10:30.
const HALTED: MarketId = 9;

#[test]
fn a_quote_stamped_before_the_open_never_fills_at_the_open() {
    let (mut h, a) = stop_over_the_close(Harness::v21());
    // Wednesday's quotes at 180 arrive 100ms before the open and are refused.
    h.advance_to(ny(2026, 10, 21, 9, 30) - 100);
    let ev = h.price(AAPL, "180");
    assert_eq!(closed_refusals(&ev), 2, "{ev:?}");

    // The open fills nothing: the quotes the market holds came after it.
    h.advance(100);
    let ev = h.cmd(Command::Tick);
    assert_eq!(ev[0], Event::MarketOpened { market: AAPL });
    assert!(fills(&ev).is_empty(), "{ev:?}");
    assert_eq!(h.engine.accounts()[&a].orders.len(), 1);

    // The first quote after the open fills the stop, gap included.
    h.advance(10);
    let fill = fills(&h.price(AAPL, "175"))[0].clone();
    assert_eq!((fill.account, fill.price), (a, f("175")));
}

#[test]
fn before_v21_a_quote_from_before_the_open_fills_at_the_open() {
    let (mut h, a) = stop_over_the_close(Harness::v17());
    h.advance_to(ny(2026, 10, 21, 9, 30) - 100);
    assert_eq!(closed_refusals(&h.price(AAPL, "180")), 0);
    h.advance(100);
    let fill = fills(&h.cmd(Command::Tick))[0].clone();
    assert_eq!((fill.account, fill.price), (a, f("180")));
}

#[test]
fn quotes_stamped_while_closed_are_refused_and_never_held() {
    let (mut h, _) = stop_over_the_close(Harness::v21());
    h.advance_to(ny(2026, 10, 20, 18, 0));
    let ev = h.price(AAPL, "150");
    assert_eq!(closed_refusals(&ev), 2, "{ev:?}");
    assert!(h.engine.markets()[&AAPL].quotes.is_empty());

    // A venue stamp inside the open counts from the sequenced time, still before it.
    h.advance_to(ny(2026, 10, 21, 9, 30) - 200);
    let quotes = vec![quote("venue0", "150", ny(2026, 10, 21, 9, 30) + 500)];
    let ev = h.cmd(Command::Prices { market: AAPL, quotes });
    assert_eq!(closed_refusals(&ev), 1, "{ev:?}");
}

#[test]
fn a_venue_stamp_after_the_close_is_refused_though_sequenced_before_it() {
    let mut h = Harness::v21();
    // The host clock trails the venue by 600ms across Tuesday's 16:00 close.
    h.advance_to(ny(2026, 10, 20, 16, 0) - 400);
    let quotes = vec![quote("venue0", "200", ny(2026, 10, 20, 16, 0) + 200)];
    let ev = h.cmd(Command::Prices { market: AAPL, quotes });
    assert_eq!(closed_refusals(&ev), 1, "{ev:?}");
    assert!(h.engine.markets()[&AAPL].quotes.is_empty());
}

#[test]
fn a_quote_arriving_during_a_closure_is_refused_though_stamped_before_it() {
    let mut h = halted(Harness::v21());
    h.deposit(acct(1), "100000");
    h.advance_to(ny(2026, 10, 20, 10, 30));
    assert!(h.cmd(Command::Tick).contains(&Event::MarketClosed { market: HALTED }));
    // Stamped a second before the halt, delayed into it.
    h.advance(500);
    let quotes = vec![quote("venue0", "100", h.ts - 1_500), quote("venue1", "100", h.ts - 1_500)];
    let ev = h.cmd(Command::Prices { market: HALTED, quotes });
    assert_eq!(closed_refusals(&ev), 2, "{ev:?}");
    h.advance_to(ny(2026, 10, 20, 10, 31));
    let ev = h.market_order(acct(1), HALTED, Side::Buy, "1", "100");
    assert_eq!(rejection(&ev), &Reject::NoPrice { error: PriceError::Stale { fresh: 0, needed: 2 } });
}

#[test]
fn a_quote_stamped_before_a_halt_and_sequenced_after_it_is_refused() {
    let mut h = halted(Harness::v21());
    h.advance_to(ny(2026, 10, 20, 10, 31) + 500);
    let stamped = ny(2026, 10, 20, 10, 30) - 1_000;
    let quotes = vec![quote("venue0", "100", stamped), quote("venue1", "100", stamped)];
    let ev = h.cmd(Command::Prices { market: HALTED, quotes });
    assert_eq!(closed_refusals(&ev), 2, "{ev:?}");
    assert!(h.engine.markets()[&HALTED].quotes.is_empty());
}

#[test]
fn quotes_held_across_a_halt_no_command_saw_are_dropped() {
    let mut h = halted(Harness::v21());
    h.deposit(acct(1), "100000");
    h.advance_to(ny(2026, 10, 20, 10, 30) - 1_000);
    h.price(HALTED, "100");
    // The next command comes after the halt has ended, so no command saw it close.
    h.advance_to(ny(2026, 10, 20, 10, 31) + 500);
    let ev = h.market_order(acct(1), HALTED, Side::Buy, "1", "100");
    assert_eq!(rejection(&ev), &Reject::NoPrice { error: PriceError::Stale { fresh: 0, needed: 2 } });
    assert!(h.engine.markets()[&HALTED].quotes.is_empty());
}

#[test]
fn activating_v21_drops_quotes_held_from_outside_the_open_session() {
    let mut h = Harness::v17();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
    let (mut h, a) = stop_over_the_close(h);
    // Under V18, quotes at 180 arrive 100ms before the open and are held.
    h.advance_to(ny(2026, 10, 21, 9, 30) - 100);
    h.price(AAPL, "180");
    assert_eq!(h.engine.markets()[&AAPL].quotes.len(), 2);
    h.advance(50);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V21 });
    assert!(h.engine.markets()[&AAPL].quotes.is_empty());
    h.advance(50);
    let ev = h.cmd(Command::Tick);
    assert!(fills(&ev).is_empty() && h.engine.accounts()[&a].orders.len() == 1, "{ev:?}");
}

#[test]
fn a_close_drops_the_quotes_so_a_reopening_waits_for_new_ones() {
    let mut h = halted(Harness::v21());
    h.deposit(acct(1), "100000");
    // Quotes stamped a second before the halt would still be fresh after it.
    h.advance_to(ny(2026, 10, 20, 10, 30) - 1_000);
    h.price(HALTED, "100");
    h.advance_to(ny(2026, 10, 20, 10, 30));
    let ev = h.cmd(Command::Tick);
    assert!(ev.contains(&Event::MarketClosed { market: HALTED }), "{ev:?}");
    assert!(h.engine.markets()[&HALTED].quotes.is_empty());

    h.advance_to(ny(2026, 10, 20, 10, 31));
    let ev = h.market_order(acct(1), HALTED, Side::Buy, "1", "100");
    assert_eq!(rejection(&ev), &Reject::NoPrice { error: PriceError::Stale { fresh: 0, needed: 2 } });
    h.price(HALTED, "101");
    let ev = h.market_order(acct(1), HALTED, Side::Buy, "1", "101");
    assert_eq!(fills(&ev)[0].price, f("101"));
}

#[test]
fn before_v21_quotes_held_over_a_halt_still_price_after_it() {
    let mut h = halted(Harness::v17());
    h.deposit(acct(1), "100000");
    h.advance_to(ny(2026, 10, 20, 10, 30) - 1_000);
    h.price(HALTED, "100");
    h.advance_to(ny(2026, 10, 20, 10, 30));
    assert!(h.cmd(Command::Tick).contains(&Event::MarketClosed { market: HALTED }));
    h.advance_to(ny(2026, 10, 20, 10, 31));
    let ev = h.market_order(acct(1), HALTED, Side::Buy, "1", "100");
    assert_eq!(fills(&ev)[0].price, f("100"));
}

/// Quotes `venue0` and `venue1` both stamped `stamped`, priced 100.
fn aged_quotes(stamped: i64) -> Command {
    Command::Prices { market: HALTED, quotes: vec![quote("venue0", "100", stamped), quote("venue1", "100", stamped)] }
}

/// Whether `HALTED` has an index at `h`'s time.
fn priced(h: &Harness) -> bool {
    h.engine.markets()[&HALTED].index(h.ts).is_ok()
}

/// Holds two quotes `age_min` minutes old at 11:29 (the market is open throughout, so `V21` holds
/// them even over the one-hour window), relists with `max_age_ms`, and reports whether the market
/// has an index 46 minutes later.
fn relisted_prices(mut h: Harness, age_min: i64, max_age_ms: i64) -> bool {
    h.advance_to(ny(2026, 10, 20, 11, 29));
    h.cmd(aged_quotes(h.ts - age_min * 60_000));
    relist(&mut h, max_age_ms);
    h.advance_to(ny(2026, 10, 20, 12, 15));
    priced(&h)
}

#[test]
fn from_v33_a_relisting_cannot_bring_a_stale_quote_back_to_pricing() {
    let (hour, two_hours) = (3_600_000, 2 * 3_600_000);
    // A quote 62 minutes old under the one-hour window is held by `V21`; raising the window to two
    // hours brings it back to pricing there, and not from `V33`.
    assert!(relisted_prices(halted(Harness::v21()), 62, two_hours));
    assert!(!relisted_prices(halted(Harness::v33()), 62, two_hours));

    // A quote that ages past the window after it was stored is stale when the relisting arrives.
    for (version, revived) in [(Harness::v21(), true), (Harness::v33(), false)] {
        let mut h = halted(version);
        h.advance_to(ny(2026, 10, 20, 11, 0));
        h.price(HALTED, "100");
        h.advance_to(ny(2026, 10, 20, 12, 5));
        assert!(!priced(&h));
        relist(&mut h, two_hours);
        assert_eq!(priced(&h), revived);
    }

    // Quotes still fresh under the old window survive a raise; a lowered window judges them as it
    // always has. Both limits are covered.
    assert!(relisted_prices(halted(Harness::v33()), 30, two_hours));
    assert!(!relisted_prices(halted(Harness::v33()), 30, hour / 4));
    assert!(!relisted_prices(halted(Harness::v33()), 62, hour));
}

#[test]
fn a_relisting_that_raises_max_age_prices_quotes_arriving_under_it() {
    let mut h = halted(Harness::v33());
    h.advance_to(ny(2026, 10, 20, 11, 0));
    relist(&mut h, 2 * 3_600_000);
    h.advance_to(ny(2026, 10, 20, 12, 30));
    h.cmd(aged_quotes(h.ts - 75 * 60_000));
    assert!(priced(&h));
}

#[test]
fn after_v33_a_long_gap_checks_for_a_closure_only_within_the_window_and_at_its_end() {
    let mut h = halted(Harness::v33());
    h.advance_to(ny(2026, 10, 20, 14, 30));
    h.price(HALTED, "100");
    // A three-day gap crosses the 16:00 close, more than the one-hour window after the quotes, with
    // both ends open. The quotes are stale by then and cannot price, so `V33` keeps them instead of
    // walking every minute of the gap.
    h.advance_to(ny(2026, 10, 23, 14, 29));
    h.cmd(Command::Tick);
    assert_eq!(h.engine.markets()[&HALTED].quotes.len(), 2);
    assert!(!priced(&h));

    // The kept quotes predate a closure, so a relisting that raises the window past their age must
    // not make them count.
    relist(&mut h, 5 * 86_400_000);
    assert!(h.engine.markets()[&HALTED].quotes.is_empty());
    assert!(!priced(&h));

    // Back at the one-hour window, quotes priced in an open session are fresh through the first hour
    // of a long gap, so only its end, with the market closed, can drop them.
    relist(&mut h, 3_600_000);
    h.advance_to(ny(2026, 10, 26, 14, 0));
    h.price(HALTED, "100");
    assert_eq!(h.engine.markets()[&HALTED].quotes.len(), 2);
    h.advance_to(ny(2026, 10, 26, 17, 30));
    h.cmd(Command::Tick);
    assert!(h.engine.markets()[&HALTED].quotes.is_empty());
}

#[test]
fn from_v33_a_closure_within_the_window_drops_quotes_though_the_gap_is_long() {
    // The quotes are still fresh at a catch-up boundary inside the gap, so a closure they predate
    // must drop them before it, as it does for a short gap.
    let mut h = halted(Harness::v33());
    h.advance_to(ny(2026, 10, 20, 10, 29) + 59_000);
    h.price(HALTED, "100");
    h.advance_to(ny(2026, 10, 23, 10, 29) + 59_000);
    h.cmd(Command::Tick);
    assert!(h.engine.markets()[&HALTED].quotes.is_empty());
}

#[test]
fn a_gap_within_the_window_still_drops_quotes_across_a_closure_from_v33() {
    let mut h = halted(Harness::v33());
    h.advance_to(ny(2026, 10, 20, 10, 30) - 1_000);
    h.price(HALTED, "100");
    h.advance_to(ny(2026, 10, 20, 10, 31) + 500);
    h.cmd(Command::Tick);
    assert!(h.engine.markets()[&HALTED].quotes.is_empty());
}

#[test]
fn before_v33_a_long_gap_walks_every_minute_and_drops_quotes_across_the_closure() {
    let mut h = halted(Harness::v21());
    h.advance_to(ny(2026, 10, 20, 10, 29) + 59_000);
    h.price(HALTED, "100");
    h.advance_to(ny(2026, 10, 23, 10, 29) + 59_000);
    h.cmd(Command::Tick);
    assert!(h.engine.markets()[&HALTED].quotes.is_empty());
}

// ── Helpers ──

/// Relists `HALTED` with the same spec but a `max_age_ms` of `max_age`.
fn relist(h: &mut Harness, max_age: i64) {
    let spec = h.engine.markets()[&HALTED].spec.clone();
    let price_rules = PriceRules { max_age_ms: max_age, ..spec.price_rules };
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { price_rules, ..spec }));
}

/// AAPL under `h`'s rules: acct(1) is long 10 at 200 with a sell stop at 190 resting over Tuesday's
/// close, which has been processed.
fn stop_over_the_close(mut h: Harness) -> (Harness, AccountId) {
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(AAPL, "200");
    h.market_order(a, AAPL, Side::Buy, "10", "200");
    h.order(a, AAPL, Side::Sell, "10", OrderKind::Stop { price: f("190") });
    h.advance_to(ny(2026, 10, 20, 16, 30));
    h.cmd(Command::Tick);
    (h, a)
}

/// `h` with `HALTED` listed: US hours, a closure 10:30-10:31 on Tue 20 Oct 2026, and quotes fresh
/// for an hour, so only the close can stop a quote from before the closure pricing after it.
fn halted(mut h: Harness) -> Harness {
    let at = |s| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").expect("a local time");
    let Schedule::Weekly { tz, windows, closed_dates, .. } = us_equities() else { unreachable!("weekly") };
    let closures = vec![Period { from: at("2026-10-20 10:30"), to: at("2026-10-20 10:31") }];
    let schedule = Schedule::Weekly { tz, windows, closed_dates, closures, special_sessions: vec![] };
    let spec = market(HALTED, "HALT", AssetClass::Equity, schedule, 2_000);
    let price_rules = PriceRules { max_age_ms: 3_600_000, ..spec.price_rules };
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { price_rules, ..spec }));
    h
}

/// How many quotes `events` refuse as stamped or sequenced outside one open session.
fn closed_refusals(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::PricesRejected { reason, .. } if reason.contains("outside one open session")))
        .count()
}
