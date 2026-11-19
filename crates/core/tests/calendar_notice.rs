//! A listed market's calendar, from `V82`: the operator's relisting may not shut a listed market within
//! seven days in hours its calendar in force has it open, whatever its weekend margin and whether or not
//! it holds positions. A held market's mark and close-outs carry on until a closure's notice has run, and
//! a position opened later meets no closure the log did not carry a week ahead. Added hours and hours shut
//! only from seven days on apply at once, and a first listing takes any calendar. A passed vote's
//! calendar change is in `parameter_votes/listing.rs`.

mod common;

use chrono::{NaiveDate, NaiveDateTime};
use chrono_tz::Tz;
use common::close_spread::{NOTICE_MS, trade};
use common::events::only_fill;
use common::weekend::{GOLD, OIL, OIL_WEEKEND_BPS, client, genesis_at, globex};
use common::{AAPL, Client, Harness, USDJPY, f, ny, us_equities};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::{Period, Schedule};
use engine_core::types::{MarketId, Side};

/// The refusal a calendar change that shuts a listed market's open hours meets.
const REFUSED: &str = "within 7 days in hours its calendar has it open";
/// The client the helpers trade through.
const SEED: u8 = 2;

#[test]
fn a_held_market_shuts_only_once_its_notice_has_run_and_its_mark_and_close_outs_carry_on_until_then() {
    // A market with no weekend margin, as at launch, and a $4,000 AAPL long on $1,000, at its 20% margin.
    let mut h = aapl(false);
    assert_eq!(h.engine.markets()[&AAPL].spec.weekend_margin_bps, 0);
    let trader = client(&mut h, SEED, "1000");
    h.price(AAPL, "200");
    only_fill(&trade(&mut h, &trader, AAPL, Side::Buy, "20", "200"));

    // A calendar shut now is refused, and AAPL stays open.
    rejected(&h.operate(relist(&h, AAPL, shut_now())), REFUSED);
    assert_eq!(h.engine.markets()[&AAPL].spec.schedule, us_equities());
    // One that shuts it from seven days on applies at once: the log carries the closure a week ahead.
    let week_on = ny(2026, 10, 27, 10, 0);
    assert_eq!(week_on, h.ts + NOTICE_MS);
    let noticed = with(vec![period("2026-10-27 10:00", "2026-10-28 16:00")], vec![]);
    applied(&h.operate(relist(&h, AAPL, noticed.clone())));
    assert_eq!(h.engine.markets()[&AAPL].spec.schedule, noticed);

    // Until then the venues price it: a 25% fall marks the long down past its equity, and it closes out.
    let events = h.quotes(AAPL, &["150", "150"]);
    assert!(events.iter().any(|event| matches!(event, Event::CloseOut { .. })), "{events:?}");
    assert_eq!(h.engine.markets()[&AAPL].mark, Some(f("150")));
    assert!(!h.engine.accounts()[&trader.account].positions.contains_key(&AAPL));
    // The market shuts when the noticed closure starts.
    h.advance_to(week_on - 60_000);
    h.cmd(Command::Tick);
    h.advance_to(week_on);
    let events = h.cmd(Command::Tick);
    assert!(events.contains(&Event::MarketClosed { market: AAPL }), "{events:?}");
}

#[test]
fn a_change_that_shuts_a_listed_market_in_open_hours_within_seven_days_is_refused_held_or_not() {
    let tomorrow = NaiveDate::from_ymd_opt(2026, 10, 21).unwrap();
    let cases = [
        ("shut now", shut_now()),
        ("a holiday tomorrow", holiday(tomorrow)),
        ("an early close today", with(vec![period("2026-10-20 15:00", "2026-10-20 16:00")], vec![])),
        ("a closure a minute inside the week", with(vec![period("2026-10-27 09:59", "2026-10-27 16:00")], vec![])),
        (
            "a half-minute halt from a nanosecond past a minute",
            with(vec![period("2026-10-21 13:00:00.000000001", "2026-10-21 13:00:30")], vec![]),
        ),
        ("each session half an hour shorter", hours(chrono_tz::America::New_York, "15:30")),
        ("the hours an hour later, in Chicago", hours(chrono_tz::America::Chicago, "16:00")),
    ];
    for held in [false, true] {
        for (case, schedule) in &cases {
            let mut h = aapl(held);
            let events = h.operate(relist(&h, AAPL, schedule.clone()));
            assert!(rejected_with(&events, REFUSED), "{case}, held {held}: {events:?}");
            assert_eq!(h.engine.markets()[&AAPL].spec.schedule, us_equities(), "{case}, held {held}");
        }
    }
}

#[test]
fn hours_added_and_hours_shut_from_seven_days_on_apply_at_once_held_or_not() {
    let cases = [
        ("each session longer", hours(chrono_tz::America::New_York, "20:00")),
        ("a Saturday session", with(vec![], vec![period("2026-10-24 10:00", "2026-10-24 12:00")])),
        ("a closure from seven days on", with(vec![period("2026-10-27 10:00", "2026-10-27 16:00")], vec![])),
        ("around the clock", Schedule::AlwaysOpen),
    ];
    for held in [false, true] {
        for (case, schedule) in &cases {
            let mut h = aapl(held);
            let events = h.operate(relist(&h, AAPL, schedule.clone()));
            assert_eq!(events.last(), Some(&Event::AdminApplied), "{case}, held {held}: {events:?}");
            assert_eq!(&h.engine.markets()[&AAPL].spec.schedule, schedule, "{case}, held {held}");
        }
    }
}

#[test]
fn a_position_opened_after_a_refused_closure_finds_its_market_priced_when_the_closure_would_have_started() {
    // AAPL holds nothing, and the operator's relisting shuts it from tomorrow's open: refused.
    let mut h = aapl(false);
    let tomorrow = with(vec![period("2026-10-21 09:30", "2026-10-21 16:00")], vec![]);
    rejected(&h.operate(relist(&h, AAPL, tomorrow)), REFUSED);
    // A client then goes long $4,000 on $1,000, at AAPL's 20% margin.
    let trader = client(&mut h, SEED, "1000");
    h.price(AAPL, "200");
    only_fill(&trade(&mut h, &trader, AAPL, Side::Buy, "20", "200"));
    // Tomorrow, when the refused closure would have run, a 25% fall reaches the engine and closes it out.
    h.advance_to(ny(2026, 10, 21, 10, 30));
    let events = h.quotes(AAPL, &["150", "150"]);
    assert!(events.iter().any(|event| matches!(event, Event::CloseOut { .. })), "{events:?}");
    assert_eq!(h.engine.markets()[&AAPL].mark, Some(f("150")));
}

#[test]
fn a_closure_moves_at_once_only_where_it_shuts_no_open_hours_within_seven_days() {
    // Thursday 29 October's early close from 14:00, set nine days ahead.
    let mut h = aapl(true);
    let early = |day: &str, from: &str| with(vec![period(&format!("{day} {from}"), &format!("{day} 16:00"))], vec![]);
    applied(&h.operate(relist(&h, AAPL, early("2026-10-29", "14:00"))));
    // Monday 26 October: the early close is three days ahead.
    h.advance_to(ny(2026, 10, 26, 10, 0));
    h.cmd(Command::Tick);
    // Moved a day later, it shuts Friday's open hours.
    rejected(&h.operate(relist(&h, AAPL, early("2026-10-30", "14:00"))), REFUSED);
    // Starting an hour later only opens hours, and so does moving it a week on, past the notice.
    applied(&h.operate(relist(&h, AAPL, early("2026-10-29", "15:00"))));
    applied(&h.operate(relist(&h, AAPL, early("2026-11-05", "14:00"))));
    // Brought back to Thursday 29 October, it shuts hours the calendar now has open.
    rejected(&h.operate(relist(&h, AAPL, early("2026-10-29", "15:00"))), REFUSED);
    assert_eq!(h.engine.markets()[&AAPL].spec.schedule, early("2026-11-05", "14:00"));
}

#[test]
fn a_closure_under_way_may_end_sooner_but_not_run_past_its_reopening() {
    let mut h = aapl(true);
    // Tuesday evening: AAPL is shut until Wednesday's 09:30 open.
    h.advance_to(ny(2026, 10, 20, 17, 0));
    h.cmd(Command::Tick);
    let later = with(vec![period("2026-10-21 09:30", "2026-10-21 11:00")], vec![]);
    rejected(&h.operate(relist(&h, AAPL, later)), REFUSED);
    let evening = with(vec![], vec![period("2026-10-20 17:00", "2026-10-20 18:00")]);
    let events = h.operate(relist(&h, AAPL, evening));
    assert!(events.contains(&Event::MarketOpened { market: AAPL }), "{events:?}");
    applied(&events);
}

#[test]
fn a_relisting_of_several_markets_waits_for_the_notice_too() {
    // Oil and gold relisted together, as an offset group's calendar moves: a two-hour closure tomorrow
    // is refused for both, and the same closure a week and a day on applies to both.
    let mut h = aapl(false);
    let both = |h: &Harness, schedule: &Schedule| AdminCommand::RelistMarkets {
        specs: [OIL, GOLD]
            .map(|id| MarketSpec { schedule: schedule.clone(), ..h.engine.markets()[&id].spec.clone() })
            .to_vec(),
    };
    let tomorrow = globex(vec![period("2026-10-21 10:00", "2026-10-21 12:00")]);
    rejected(&h.operate(both(&h, &tomorrow)), REFUSED);
    assert_eq!(h.engine.markets()[&OIL].spec.schedule, globex(vec![]));
    let week_on = globex(vec![period("2026-10-28 10:00", "2026-10-28 12:00")]);
    applied(&h.operate(both(&h, &week_on)));
    for id in [OIL, GOLD] {
        assert_eq!(h.engine.markets()[&id].spec.schedule, week_on, "market {id}");
    }
}

#[test]
fn a_first_listing_takes_any_calendar_and_a_listed_market_waits_held_or_not() {
    // Genesis lists oil with a calendar shut at once.
    let mut h = genesis_at(ProtocolVersion::LATEST, shut_now(), 0, 0);
    assert_eq!(h.engine.markets()[&OIL].spec.schedule, shut_now());
    // Once listed, USD-JPY and AAPL, which hold nothing, and gold, which holds a long, each wait.
    hold(&mut h, GOLD);
    for market in [USDJPY, AAPL, GOLD] {
        let before = h.engine.markets()[&market].spec.clone();
        rejected(&h.operate(relist(&h, market, shut_now())), REFUSED);
        assert_eq!(h.engine.markets()[&market].spec, before);
    }
}

#[test]
fn the_bound_holds_whatever_the_weekend_margin_and_applies_from_v82() {
    // Gold with no weekend margin and oil with one, each held, meet it for a two-hour closure tomorrow,
    // too short to be a weekend gap.
    let closure_tomorrow = globex(vec![period("2026-10-21 10:00", "2026-10-21 12:00")]);
    let mut h = genesis_at(ProtocolVersion::LATEST, globex(vec![]), OIL_WEEKEND_BPS, 0);
    for market in [GOLD, OIL] {
        hold(&mut h, market);
        rejected(&h.operate(relist(&h, market, closure_tomorrow.clone())), REFUSED);
    }
    // Before `V82` each applies at once, so a log written under earlier rules replays.
    let mut h = genesis_at(ProtocolVersion::V80, globex(vec![]), OIL_WEEKEND_BPS, 0);
    for market in [GOLD, OIL, AAPL] {
        hold(&mut h, market);
    }
    for market in [GOLD, OIL] {
        applied(&h.operate(relist(&h, market, closure_tomorrow.clone())));
    }
    applied(&h.operate(relist(&h, AAPL, shut_now())));
}

// ── Helpers ──

/// A `LATEST` genesis with no weekend margins, and if `held` client `SEED` long 20 AAPL at 200.
fn aapl(held: bool) -> Harness {
    let mut h = genesis_at(ProtocolVersion::LATEST, globex(vec![]), 0, 0);
    if held {
        hold(&mut h, AAPL);
    }
    h
}

/// Client `SEED`, funded with $100,000 on its first call, long 20 of `market` at 200.
fn hold(h: &mut Harness, market: MarketId) {
    let funded = h.engine.accounts().contains_key(&trader(h).account);
    let holder = if funded { trader(h) } else { client(h, SEED, "100000") };
    h.price(market, "200");
    only_fill(&trade(h, &holder, market, Side::Buy, "20", "200"));
}

/// Client `SEED`, signing for the next minute.
fn trader(h: &Harness) -> Client {
    Client { expires_at: h.ts + 60_000, ..Client::new(SEED) }
}

/// `market`'s listing in force with `schedule`.
fn relist(h: &Harness, market: MarketId, schedule: Schedule) -> AdminCommand {
    AdminCommand::UpsertMarket(MarketSpec { schedule, ..h.engine.markets()[&market].spec.clone() })
}

/// A weekly calendar whose only hours are Sunday 00:00 to 00:01 New York time, shut at the harness start.
fn shut_now() -> Schedule {
    serde_json::from_value(serde_json::json!({
        "kind": "weekly", "tz": "America/New_York",
        "windows": [{ "open": "Sun 00:00", "close": "Sun 00:01" }],
    }))
    .unwrap()
}

/// Weekday hours from 09:30 to `close` in `tz`.
fn hours(tz: Tz, close: &str) -> Schedule {
    let windows: Vec<_> = ["Mon", "Tue", "Wed", "Thu", "Fri"]
        .iter()
        .map(|d| serde_json::json!({ "open": format!("{d} 09:30"), "close": format!("{d} {close}") }))
        .collect();
    serde_json::from_value(serde_json::json!({ "kind": "weekly", "tz": tz.name(), "windows": windows })).unwrap()
}

/// US equity hours with dated `closures` and `special_sessions`.
fn with(closures: Vec<Period>, special_sessions: Vec<Period>) -> Schedule {
    let Schedule::Weekly { tz, windows, closed_dates, .. } = us_equities() else { unreachable!("weekly hours") };
    Schedule::Weekly { tz, windows, closed_dates, closures, special_sessions }
}

/// US equity hours with `date` a holiday.
fn holiday(date: NaiveDate) -> Schedule {
    let Schedule::Weekly { tz, windows, .. } = us_equities() else { unreachable!("weekly hours") };
    Schedule::Weekly { tz, windows, closed_dates: vec![date], closures: vec![], special_sessions: vec![] }
}

/// A dated period in the calendar's local time, `"YYYY-MM-DD HH:MM[:SS[.fraction]]"`.
fn period(from: &str, to: &str) -> Period {
    let at = |s: &str| {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
            .unwrap()
    };
    Period { from: at(from), to: at(to) }
}

fn applied(events: &[Event]) {
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
}

fn rejected(events: &[Event], reason: &str) {
    assert!(rejected_with(events, reason), "{events:?}");
}

fn rejected_with(events: &[Event], reason: &str) -> bool {
    matches!(events.last(), Some(Event::AdminRejected { reason: r }) if r.contains(reason))
}
