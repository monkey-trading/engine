//! The rollover's bounds from `V76`, through `Engine::apply`: the operator moves the rollover time only
//! as a seven-day notice names it, a new time never brings the next rollover earlier than the one due,
//! and a rollover charges for the time since the last. So moving the rollover minute by
//! minute charges nothing more, and financing never runs faster than a day's per day. The activation
//! takes the rollover before the one due as the last, so a daylight saving day charges its own hours.

mod common;

use chrono::{TimeZone, Timelike};
use common::close_spread::trade;
use common::events::only_fill;
use common::weekend::{client, genesis_at, globex};
use common::{BTC, Client, Harness, f};
use engine_core::Fixed;
use engine_core::command::AdminCommand;
use engine_core::event::Event;
use engine_core::fixed::Rounding;
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::params::{ProtocolParams, Rollover};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};

const MINUTE_MS: Millis = 60_000;
const HOUR_MS: Millis = 60 * MINUTE_MS;
const DAY_MS: Millis = 24 * HOUR_MS;

/// A fresh log at `version`: 100m of house capital credited, so the operator signs its commands, a
/// 4.3% benchmark and BTC's 2.5% markup, BTC at $100,000, and rollovers at 22:00 in London. Rotation
/// is off, as at deployment, so the operator's bounded commands apply.
fn genesis(version: ProtocolVersion) -> Harness {
    let mut h = genesis_at(version, globex(vec![]), 0, 0);
    h.price(BTC, "100000");
    h
}

/// What `account` paid in financing among `events`; negative when it received.
fn financing(events: &[Event], account: &Client) -> Fixed {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Financing { account: a, amount, .. } if *a == account.account => Some(*amount),
            _ => None,
        })
        .sum()
}

/// A rollover at the whole minute after the sequenced time, in UTC, and that minute.
fn next_minute(h: &Harness) -> (Rollover, Millis) {
    let at = (h.ts / MINUTE_MS + 1) * MINUTE_MS;
    let utc = chrono::Utc.timestamp_millis_opt(at).unwrap();
    (Rollover { tz: chrono_tz::UTC, hour: utc.hour(), minute: utc.minute() }, at)
}

/// The operator's signed `SetParams` that changes nothing but the rollover time.
fn set_rollover(h: &mut Harness, rollover: Rollover) -> Vec<Event> {
    let params = ProtocolParams { rollover, ..h.engine.params().cloned().unwrap() };
    h.operate(AdminCommand::SetParams(params))
}

/// The operator's notice of `rollover` from `effective_at`.
fn rollover_notice(rollover: Rollover, effective_at: Millis) -> AdminCommand {
    AdminCommand::AnnounceParams(ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress: None,
        score_tiers: None,
        profit_fee_curve: None,
        volume_tiers: None,
        rollover: Some(rollover),
        corporate_bounds: None,
        portfolio_grid: None,
        effective_at,
    })
}

/// The operator moves the rollover to the next minute with no notice, and the sequenced time reaches
/// that minute. Returns the move's outcome and what the price update then charged.
fn charge_again(h: &mut Harness) -> (Vec<Event>, Vec<Event>) {
    let (rollover, at) = next_minute(h);
    let moved = set_rollover(h, rollover);
    h.advance_to(at);
    (moved, h.price(BTC, "100000"))
}

fn rejected(events: &[Event], reason: &str) {
    assert!(matches!(events.last(), Some(Event::AdminRejected { reason: r }) if r.contains(reason)), "{events:?}");
}

/// Runs the scheduled rollover and returns what `account` paid in it.
fn scheduled_rollover(h: &mut Harness, account: &Client) -> Fixed {
    h.advance_to(h.engine.next_rollover().unwrap());
    financing(&h.price(BTC, "100000"), account)
}

#[test]
fn moving_the_rollover_every_minute_charges_a_long_nothing_more_within_the_day() {
    let mut h = genesis(ProtocolVersion::V76);
    let long = client(&mut h, 1, "100000");
    only_fill(&trade(&mut h, &long, BTC, Side::Buy, "1", "100000"));
    let launch = h.engine.params().unwrap().rollover.clone();

    // Control: the scheduled rollover charges one day's financing on $100,000 at 4.3% + 2.5%.
    let one_day = scheduled_rollover(&mut h, &long);
    assert_eq!(one_day, f("100000").mul_bps(680).div_int(365) + Fixed::from_raw(1), "one day at 6.8% a year");

    let started = h.ts;
    let mut charged = Fixed::ZERO;
    for _ in 0..60 {
        let (moved, priced) = charge_again(&mut h);
        rejected(&moved, "a rollover change needs a notice of at least seven days");
        charged += financing(&priced, &long);
    }
    assert!(h.ts - started <= 61 * MINUTE_MS, "sixty minutes, not {} ms", h.ts - started);
    assert_eq!(h.engine.params().unwrap().rollover, launch, "the rollover time stays");
    assert!(charged <= one_day, "the long paid {charged} in an hour, more than a day's {one_day}");
}

#[test]
fn moving_the_rollover_closes_out_no_position_within_a_day_at_an_unchanged_price() {
    let mut h = genesis(ProtocolVersion::V76);
    // $12,000 behind one BTC at 10% margin: $10,000 required, closed out below $5,000 of equity. A
    // day's financing is under $19.
    let long = client(&mut h, 1, "12000");
    only_fill(&trade(&mut h, &long, BTC, Side::Buy, "1", "100000"));
    let started = h.ts;
    let house = h.engine.house_cash();
    while h.ts - started < DAY_MS {
        charge_again(&mut h);
    }
    assert!(!h.engine.accounts()[&long.account].positions.is_empty(), "the long stands a day later");
    let paid = house - h.engine.house_cash();
    assert!(paid.abs() < f("19"), "a day's financing at most, not {paid}");
}

#[test]
fn moving_the_rollover_pays_a_short_no_more_than_a_days_benchmark_within_the_day() {
    let mut h = genesis(ProtocolVersion::V76);
    // The benchmark is above the markup, so a short receives 4.3% - 2.5% a year.
    let short = client(&mut h, 2, "100000");
    only_fill(&trade(&mut h, &short, BTC, Side::Sell, "5", "100000"));
    let one_day = -scheduled_rollover(&mut h, &short);
    assert!(one_day.is_positive(), "control: a short receives the benchmark less the markup: {one_day}");

    let (started, house, balance) = (h.ts, h.engine.house_cash(), h.balance(short.account));
    for _ in 0..60 {
        charge_again(&mut h);
    }
    let received = h.balance(short.account) - balance;
    assert_eq!(h.engine.house_cash(), house - received, "what the short received left the house");
    assert!(h.ts - started <= 61 * MINUTE_MS);
    assert!(received <= one_day, "the house paid the short {received} in an hour, more than a day's {one_day}");
}

#[test]
fn a_noticed_rollover_move_keeps_the_due_rollover_and_the_next_charges_for_the_time_since_it() {
    let mut h = genesis(ProtocolVersion::V76);
    let long = client(&mut h, 1, "100000");
    only_fill(&trade(&mut h, &long, BTC, Side::Buy, "1", "100000"));
    let earlier = Rollover { tz: chrono_tz::Europe::London, hour: 21, minute: 0 };

    // A notice takes effect no sooner than seven days on, and the move waits for it.
    let effective_at = h.ts + NOTICE_MS;
    rejected(&h.operate(rollover_notice(earlier.clone(), effective_at - 1)), "takes effect no sooner than");
    assert_eq!(h.operate(rollover_notice(earlier.clone(), effective_at)).last(), Some(&Event::AdminApplied));
    rejected(&set_rollover(&mut h, earlier.clone()), "the rollover change takes effect from");

    // Tuesday 27 October, 21:30 in London: the 22:00 rollover is due, and today's 21:00 has passed.
    let london = |d, hour, minute| chrono_tz::Europe::London.with_ymd_and_hms(2026, 10, d, hour, minute, 0).unwrap();
    h.advance_to(london(27, 21, 30).timestamp_millis());
    assert!(h.ts >= effective_at);
    h.price(BTC, "100000");
    let due = h.engine.next_rollover().unwrap();
    assert_eq!(due, london(27, 22, 0).timestamp_millis());
    assert_eq!(set_rollover(&mut h, earlier.clone()).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.params().unwrap().rollover, earlier);
    assert_eq!(h.engine.next_rollover(), Some(due), "the move keeps the due rollover, neither earlier nor skipped");
    assert!(h.engine.operator_bounds().notice.is_none(), "the move spends the notice");

    // The due rollover charges a day; the next, 23 hours on at 21:00, charges 23 hours of one.
    let one_day = scheduled_rollover(&mut h, &long);
    assert_eq!(h.engine.next_rollover(), Some(london(28, 21, 0).timestamp_millis()));
    let short_day = scheduled_rollover(&mut h, &long);
    let hours = |n| Fixed::from_int(n * HOUR_MS);
    assert_eq!(short_day, one_day.mul_div(hours(23), hours(24), Rounding::Up));
    assert!(short_day < one_day);
    // A day on, a whole day again.
    assert_eq!(scheduled_rollover(&mut h, &long), one_day);
}

#[test]
fn before_v76_the_rollover_moves_at_once_and_a_notice_cannot_name_one() {
    let mut h = genesis(ProtocolVersion::V74);
    let (rollover, at) = next_minute(&h);
    rejected(&h.operate(rollover_notice(rollover.clone(), h.ts + NOTICE_MS)), "a rollover notice needs protocol V76");
    assert_eq!(set_rollover(&mut h, rollover).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.next_rollover(), Some(at));
}

#[test]
fn v76s_activation_takes_the_rollover_before_the_one_due_so_a_daylight_saving_day_charges_its_hours() {
    let mut h = genesis(ProtocolVersion::V74);
    let long = client(&mut h, 1, "100000");
    only_fill(&trade(&mut h, &long, BTC, Side::Buy, "1", "100000"));
    let one_day = scheduled_rollover(&mut h, &long);
    // Sunday 28 March 2027, noon in London: the clocks went forward this morning, so the 22:00 rollover
    // due tonight comes 23 hours after last night's.
    let london =
        |d, hour| chrono_tz::Europe::London.with_ymd_and_hms(2027, 3, d, hour, 0, 0).unwrap().timestamp_millis();
    h.advance_to(london(28, 12));
    h.price(BTC, "100000");
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V76 });
    assert_eq!(h.engine.operator_bounds().last_rollover, Some(london(27, 22)));
    assert_eq!(
        scheduled_rollover(&mut h, &long),
        one_day.mul_div(Fixed::from_int(23), Fixed::from_int(24), Rounding::Up)
    );
    assert_eq!(scheduled_rollover(&mut h, &long), one_day);
}

#[test]
fn a_rollover_after_the_clocks_go_back_charges_its_twenty_five_hours() {
    let mut h = genesis(ProtocolVersion::V76);
    let long = client(&mut h, 1, "100000");
    only_fill(&trade(&mut h, &long, BTC, Side::Buy, "1", "100000"));
    // Tuesday 20 to Saturday 24 October at 22:00 in London, a day apart.
    let one_day = scheduled_rollover(&mut h, &long);
    for _ in 0..4 {
        assert_eq!(scheduled_rollover(&mut h, &long), one_day);
    }
    // The clocks go back on Sunday 25 October, so its 22:00 comes 25 hours after Saturday's.
    let hours = |n| Fixed::from_int(n * HOUR_MS);
    assert_eq!(scheduled_rollover(&mut h, &long), one_day.mul_div(hours(25), hours(24), Rounding::Up));
}
