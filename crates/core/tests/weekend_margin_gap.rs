//! What `V59`'s weekend margin holds over the gap, through `Engine::apply`: from two hours before a
//! market's weekend gap starts until it reopens, withdrawals and risk added in any market keep each
//! position in it at its weekend margin; a closure longer than a weekend holds it to the reopening;
//! a search that finds no gap keeps a frontier where it runs again in time for a gap's lead; a noticed
//! weekend margin that applies inside the gap closes it out then, once; a market quoted in yen meets
//! and closes out at its weekend margin in dollars; and a catch-up deferred past a weekend close still
//! closes out at it.

mod common;

use common::close_spread::{NOTICE_MS, trade};
use common::events::{fills, only_fill, rejection};
use common::weekend::{NIKKEI, OIL, OIL_WEEKEND_BPS, chicago, client, genesis_v59, globex, period, withdraw};
use common::{BTC, Harness, USDJPY, f};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Reject};
use engine_core::fixed::Rounding;
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::MarketSpec;
use engine_core::session::{Period, Schedule};
use engine_core::types::{Millis, Side};
use engine_core::weekend::{WEEKEND_LEAD_MS, WeekendGap};

const HOUR_MS: Millis = 60 * 60 * 1000;
const DAY_MS: Millis = 24 * HOUR_MS;

#[test]
fn over_the_gap_withdrawals_and_new_risk_in_any_market_keep_the_weekend_margin() {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, 0);
    let b = client(&mut h, 2, "60000");
    h.price(OIL, "70");
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &b, OIL, Side::Buy, "10000", "70"));
    // From the lead, b's equity, about $59.5k after three nights' financing, must keep 7.5% of its $700k
    // of oil: $52.5k. A $100k BTC long at 10% would need $10k more, and only the rest may leave.
    let close = chicago(2026, 10, 23, 16, 0);
    h.advance_to(close - WEEKEND_LEAD_MS);
    h.price(OIL, "70");
    h.price(BTC, "100000");
    assert!(matches!(rejection(&trade(&mut h, &b, BTC, Side::Buy, "1", "100000")), Reject::WeekendMargin { .. }));
    let spare = |h: &Harness| h.equity_and_margin(b.account).0 - f("52500");
    assert!(spare(&h) > f("6000") && spare(&h) < f("7000"), "{}", spare(&h));
    assert_eq!(refused(&withdraw(&mut h, &b, "7000")), Reject::InsufficientFunds { available: spare(&h) });
    assert!(withdrawn(&withdraw(&mut h, &b, "6000")));
    // On Saturday, inside the gap, the same holds: b keeps its long, with under $1,000 to spare.
    h.advance_to(close + 20 * HOUR_MS);
    assert!(fills(&h.cmd(Command::Tick)).is_empty());
    h.price(BTC, "100000");
    assert!(matches!(rejection(&trade(&mut h, &b, BTC, Side::Buy, "1", "100000")), Reject::WeekendMargin { .. }));
    assert_eq!(refused(&withdraw(&mut h, &b, "1000")), Reject::InsufficientFunds { available: spare(&h) });
    // From Sunday's open the normal 1% applies again.
    h.advance_to(chicago(2026, 10, 25, 17, 0));
    h.price(OIL, "70");
    assert!(withdrawn(&withdraw(&mut h, &b, "40000")));
}

#[test]
fn a_long_closure_holds_the_weekend_margin_to_its_reopening() {
    // Six weeks shut from Monday 26 October at noon, after the first weekend.
    let mut h = genesis_v59(globex(vec![period("2026-10-26 12:00", "2026-12-07 16:00")]), OIL_WEEKEND_BPS, 0);
    let b = client(&mut h, 2, "60000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &b, OIL, Side::Buy, "10000", "70"));
    let (halt, reopen) = (chicago(2026, 10, 26, 12, 0), chicago(2026, 12, 7, 17, 0));
    h.advance_to(chicago(2026, 11, 15, 12, 0));
    h.cmd(Command::Tick);
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(WeekendGap { close: halt, reopen, closed_out: true }));
    assert!(matches!(refused(&withdraw(&mut h, &b, "8000")), Reject::InsufficientFunds { .. }));
    // At the reopening the next gap is the Friday after.
    h.advance_to(chicago(2026, 12, 8, 10, 0));
    h.cmd(Command::Tick);
    let next =
        WeekendGap { close: chicago(2026, 12, 11, 16, 0), reopen: chicago(2026, 12, 13, 17, 0), closed_out: false };
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(next));
}

#[test]
fn a_closure_longer_than_the_search_stays_one_gap_until_it_reopens() {
    // Shut from Monday 26 October 2026 to the Monday evening session of 15 November 2027: past the
    // 366-day search, so the gap first ends where the search does and is looked at again there.
    let mut h = genesis_v59(globex(vec![period("2026-10-26 12:00", "2027-11-15 16:00")]), OIL_WEEKEND_BPS, 0);
    let (halt, reopen) = (chicago(2026, 10, 26, 12, 0), chicago(2027, 11, 15, 17, 0));
    h.advance_to(chicago(2026, 10, 27, 12, 0));
    h.cmd(Command::Tick);
    let searched = WeekendGap { close: halt, reopen: halt + 366 * DAY_MS, closed_out: true };
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(searched));
    h.advance_to(halt + 367 * DAY_MS);
    while h.engine.catch_up_pending(h.ts) {
        h.cmd(Command::Tick);
    }
    h.cmd(Command::Tick);
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(WeekendGap { close: halt, reopen, closed_out: true }));
}

#[test]
fn a_noticed_weekend_margin_that_applies_inside_the_gap_closes_out_then_and_only_once() {
    // Oil lists with none, and on Saturday a notice gives it 7.5% from the Saturday after.
    let mut h = genesis_v59(globex(vec![]), 0, 0);
    let a = client(&mut h, 1, "20000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    h.advance_to(chicago(2026, 10, 24, 12, 0));
    let notice = ListingNotice {
        spread_tiers: None,
        margin_tiers: None,
        markup_bps: None,
        weekend_margin_bps: Some(OIL_WEEKEND_BPS),
        effective_at: h.ts + NOTICE_MS,
    };
    let events = h.operate(AdminCommand::AnnounceListing { market: OIL, notice });
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    // The last price before Friday's close is 70.5. The relisting applies on Saturday at noon, with
    // the market in its gap, and a, below 7.5% of $705k, closes out at it then.
    h.advance_to(chicago(2026, 10, 30, 15, 0));
    h.price(OIL, "70.5");
    h.advance_to(chicago(2026, 10, 31, 12, 0));
    let listing = MarketSpec { weekend_margin_bps: OIL_WEEKEND_BPS, ..oil(&h) };
    let events = h.operate(AdminCommand::UpsertMarket(listing.clone()));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert!(events.iter().any(|event| matches!(event, Event::CloseOut { account, .. } if *account == a.account)));
    let closed = only_fill(&events).clone();
    assert!(closed.liquidation && closed.index == f("70.5"), "{closed:?}");
    let gap = WeekendGap { close: h.ts, reopen: chicago(2026, 11, 1, 17, 0), closed_out: true };
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(gap));
    // The same listing again closes nothing.
    h.advance(30 * 60 * 1000);
    let events = h.operate(AdminCommand::UpsertMarket(listing));
    assert!(!events.iter().any(|event| matches!(event, Event::CloseOut { .. })), "{events:?}");
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(gap));
}

#[test]
fn a_search_that_finds_no_gap_keeps_a_frontier_where_it_runs_again_in_time_for_the_lead() {
    // Shut from noon on 26 October to noon on 29 October 2026, and next from 13:00 on 30 October 2027,
    // an hour past the 366 days a search from the first reopening covers.
    let reopen = chicago(2026, 10, 29, 12, 0);
    let horizon = reopen + 366 * DAY_MS;
    assert_eq!(horizon, chicago(2027, 10, 30, 12, 0));
    let closures = vec![period("2026-10-26 12:00", "2026-10-29 12:00"), period("2027-10-30 13:00", "2027-11-01 12:00")];
    let mut h = genesis_v59(daily(closures), OIL_WEEKEND_BPS, 0);
    let first = WeekendGap { close: chicago(2026, 10, 26, 12, 0), reopen, closed_out: false };
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(first));
    // Once it reopens, no gap starts within 366 days: the market keeps a frontier the lead before the
    // search's end, with no window.
    h.advance_to(reopen + HOUR_MS);
    h.cmd(Command::Tick);
    let frontier = horizon - WEEKEND_LEAD_MS;
    let kept = WeekendGap { close: frontier, reopen: frontier, closed_out: true };
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(kept));
    assert_eq!(h.engine.markets()[&OIL].weekend_window(), None);
    // At the frontier the search runs again, and finds the closure an hour past the horizon in time for
    // all of its lead.
    h.advance_to(frontier + 30 * 60 * 1000);
    while h.engine.catch_up_pending(h.ts) {
        h.cmd(Command::Tick);
    }
    h.cmd(Command::Tick);
    let close = chicago(2027, 10, 30, 13, 0);
    let second = WeekendGap { close, reopen: chicago(2027, 11, 1, 12, 0), closed_out: false };
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(second));
    assert_eq!(h.engine.markets()[&OIL].weekend_window().map(|window| window.from), Some(close - WEEKEND_LEAD_MS));
    assert!(h.ts < close - WEEKEND_LEAD_MS);
}

#[test]
fn a_market_quoted_in_yen_meets_and_closes_out_at_its_weekend_margin_in_dollars() {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "1000");
    h.price(USDJPY, "150");
    h.price(NIKKEI, "38000");
    // 300 at ¥38,000 is $76,000 at ¥150, on the 1% band.
    only_fill(&trade(&mut h, &a, NIKKEI, Side::Buy, "300", "38000"));
    let close = chicago(2026, 10, 23, 16, 0);
    h.advance_to(close - WEEKEND_LEAD_MS);
    h.price(USDJPY, "150");
    h.price(NIKKEI, "38000");
    // An add to 310 would need 1.5% of ¥11.78M in dollars at ¥150, each step rounded up.
    let usd = |yen: &str, fx: &str| f(yen).div_rounded(f(fx), Rounding::Up).mul_bps_rounded(150, Rounding::Up);
    let refused = rejection(&trade(&mut h, &a, NIKKEI, Side::Buy, "10", "38000")).clone();
    assert!(matches!(refused, Reject::WeekendMargin { required, .. } if required == usd("11780000", "150")));
    // The last prices before the close: ¥38,000, and USD-JPY at ¥151. At the close, a is below 1.5%
    // of ¥11.4M in dollars at ¥151, and closes out at both marks.
    h.advance_to(close - 500);
    h.price(USDJPY, "151");
    h.price(NIKKEI, "38000");
    h.advance_to(close + HOUR_MS);
    let events = h.cmd(Command::Tick);
    let close_out = events.iter().find_map(|event| match event {
        Event::CloseOut { account, margin, .. } if *account == a.account => Some(*margin),
        _ => None,
    });
    assert_eq!(close_out, Some(usd("11400000", "151")), "{events:?}");
    let closed = only_fill(&events).clone();
    assert_eq!((closed.market, closed.index, closed.fx_index), (NIKKEI, f("38000"), Some(f("151"))), "{closed:?}");
    assert!(closed.realised_pnl.is_negative(), "{closed:?}");
}

#[test]
fn a_catch_up_deferred_past_a_weekend_close_closes_out_at_it_and_agrees_with_the_bound() {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "10000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    // The next command comes 400 days on: past more rollovers than one command walks.
    h.advance(400 * DAY_MS);
    assert!(h.engine.catch_up_pending(h.ts));
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|event| matches!(event, Event::CatchUpDeferred { .. })), "deferred");
    let closed = only_fill(&events).clone();
    assert!(closed.liquidation && closed.account == a.account, "the first Friday's close-out runs: {closed:?}");
    for _ in 0..3 {
        if !h.engine.catch_up_pending(h.ts) {
            break;
        }
        assert!(fills(&h.cmd(Command::Tick)).is_empty());
    }
    assert!(!h.engine.catch_up_pending(h.ts));
    // The command the catch-up was for walks the rest, weekend boundaries included.
    h.cmd(Command::Tick);
    let gap = h.engine.markets()[&OIL].weekend.expect("oil keeps a gap");
    assert!(gap.close > h.ts || (gap.closed_out && gap.reopen > h.ts), "{gap:?} at {}", h.ts);
}

#[test]
fn a_held_market_keeping_a_frontier_takes_a_gap_only_seven_days_on() {
    let mut h = genesis_v59(daily(vec![period("2026-10-26 12:00", "2026-10-29 12:00")]), OIL_WEEKEND_BPS, 0);
    h.advance_to(chicago(2026, 10, 29, 13, 0));
    h.cmd(Command::Tick);
    assert!(h.engine.markets()[&OIL].weekend.is_some_and(|gap| gap.is_frontier()));
    let a = client(&mut h, 1, "60000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    // A closure from an hour on would close a's long out with an hour's warning, where the calendar in
    // force has no gap at all; one from eight days on gives the notice, and the market keeps it.
    let shut_from =
        |h: &Harness, from: &str, to: &str| MarketSpec { schedule: daily(vec![period(from, to)]), ..oil(h) };
    let soon = shut_from(&h, "2026-10-29 14:00", "2026-11-01 12:00");
    let reason = format!(
        "market 32 holds positions, so a calendar change may not start a weekend gap at {}, within 7 days, that its calendar does not",
        h.ts + HOUR_MS
    );
    assert_eq!(common::spreads::refused(&h.operate(AdminCommand::UpsertMarket(soon))), reason);
    let later = shut_from(&h, "2026-11-06 12:00", "2026-11-08 12:00");
    let events = h.operate(AdminCommand::UpsertMarket(later));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    let gap = WeekendGap { close: chicago(2026, 11, 6, 12, 0), reopen: chicago(2026, 11, 8, 12, 0), closed_out: false };
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(gap));
}

/// Open daily from 01:00 to 23:00 in Chicago, with `closures`.
fn daily(closures: Vec<Period>) -> Schedule {
    let windows: Vec<_> = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
        .iter()
        .map(|day| serde_json::json!({ "open": format!("{day} 01:00"), "close": format!("{day} 23:00") }))
        .collect();
    let daily = serde_json::json!({ "kind": "weekly", "tz": "America/Chicago", "windows": windows });
    let Schedule::Weekly { tz, windows, closed_dates, special_sessions, .. } = serde_json::from_value(daily).unwrap()
    else {
        unreachable!("a weekly schedule")
    };
    Schedule::Weekly { tz, windows, closed_dates, closures, special_sessions }
}

/// Oil's listing as it stands.
fn oil(h: &Harness) -> MarketSpec {
    h.engine.markets()[&OIL].spec.clone()
}

/// The reason a withdrawal was refused.
fn refused(events: &[Event]) -> Reject {
    match events.last() {
        Some(Event::WithdrawalRejected { reason, .. }) => reason.clone(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Whether a withdrawal was taken.
fn withdrawn(events: &[Event]) -> bool {
    events.iter().any(|event| matches!(event, Event::WithdrawalApproved { .. } | Event::WithdrawalQueued { .. }))
}
