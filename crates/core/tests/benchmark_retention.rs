//! Dated financing survives legal rollover rescheduling without inventing a missing predecessor:
//! calendar and time-zone changes, retention boundaries, and the earliest and latest engine stamps.

mod common;

use chrono::{DateTime, NaiveDate};
use common::{BTC, Harness, acct, f};
use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::fixed::Rounding;
use engine_core::params::Rollover;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;
use engine_core::{Engine, Fixed};

#[test]
fn snapshot_pruning_preserves_a_dated_only_predecessor_after_a_calendar_rewind() {
    snapshot_calendar_rewind(false);
}

#[test]
fn snapshot_pruning_preserves_the_predecessor_instead_of_a_conflicting_scalar() {
    snapshot_calendar_rewind(true);
}

fn snapshot_calendar_rewind(scalar: bool) {
    let mut h = fixture(scalar);
    schedule(&mut h, "Pacific/Auckland", 22, 0);
    assert_eq!(h.engine.benchmark_date(h.ts), day("2026-10-21"));
    snapshot(&mut h, &[("2026-10-19", "0.01"), ("2026-10-20", "0.02"), ("2026-10-21", "0.03")]);
    hold(&mut h);
    snapshot(&mut h, &[]);
    let before = h.balance(acct(1));
    schedule(&mut h, "Europe/London", 22, 0);
    assert_eq!(h.engine.next_rollover(), Some(stamp("2026-10-20T21:00:00Z")));
    h.advance_to(h.engine.next_rollover().unwrap());
    let events = h.cmd(Command::Tick);
    assert_charge(&h, &events, "0.02", "2026-10-20", "2026-10-20");
    assert_eq!(h.balance(acct(1)), before - charge(&h, "0.02"), "scalar={scalar}");
    assert!(amounts(&h.cmd(Command::Tick)).is_empty());
    assert_replay(&mut h);
}

#[test]
fn rollover_pruning_preserves_a_dated_only_predecessor_after_a_date_line_rewind() {
    rollover_calendar_rewind(false);
}

#[test]
fn rollover_pruning_preserves_the_predecessor_instead_of_a_conflicting_scalar() {
    rollover_calendar_rewind(true);
}

fn rollover_calendar_rewind(scalar: bool) {
    let mut h = fixture(scalar);
    h.advance_to(stamp("2026-10-21T10:00:00Z"));
    schedule(&mut h, "Pacific/Kiritimati", 0, 1);
    assert_eq!(h.engine.benchmark_date(h.ts), day("2026-10-22"));
    snapshot(
        &mut h,
        &[("2026-10-19", "0.009"), ("2026-10-20", "0.01"), ("2026-10-21", "0.02"), ("2026-10-22", "0.03")],
    );
    hold(&mut h);
    let before = h.balance(acct(1));
    h.advance_to(h.engine.next_rollover().unwrap());
    let first = h.cmd(Command::Tick);
    assert_charge(&h, &first, "0.03", "2026-10-22", "2026-10-22");

    // A restored engine has no retained cache or history beyond its serialized state.
    h.engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
    schedule(&mut h, "Etc/GMT+12", 22, 2);
    assert_eq!(h.engine.next_rollover(), Some(stamp("2026-10-21T10:02:00Z")));
    h.advance_to(h.engine.next_rollover().unwrap());
    let second = h.cmd(Command::Tick);
    assert_charge(&h, &second, "0.01", "2026-10-20", "2026-10-20");
    assert_eq!(h.balance(acct(1)), before - charge(&h, "0.03") - charge(&h, "0.01"));
    assert_eq!(history_days(&h), [day("2026-10-20"), day("2026-10-21"), day("2026-10-22")]);
    assert!(amounts(&h.cmd(Command::Tick)).is_empty());
    assert_replay(&mut h);
}

#[test]
fn weekend_retention_keeps_fridays_predecessor_instead_of_a_future_monday_rate() {
    let mut h = fixture(false);
    h.advance_to(stamp("2026-10-26T10:00:00Z"));
    schedule(&mut h, "Pacific/Kiritimati", 0, 1);
    snapshot(&mut h, &[("2026-10-22", "0.009"), ("2026-10-23", "0.01"), ("2026-10-26", "0.03")]);
    hold(&mut h);
    let before = h.balance(acct(1));
    schedule(&mut h, "Etc/GMT+12", 22, 2);
    h.advance_to(h.engine.next_rollover().unwrap());
    let events = h.cmd(Command::Tick);
    assert_charge(&h, &events, "0.01", "2026-10-23", "2026-10-25");
    assert_eq!(h.balance(acct(1)), before - charge(&h, "0.01"));
    assert_eq!(history_days(&h), [day("2026-10-23"), day("2026-10-26")]);
    assert_replay(&mut h);
}

#[test]
fn absent_predecessors_keep_missing_or_scalar_policy_without_using_future_observations() {
    for (scalar, future) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut h = fixture(scalar);
        schedule(&mut h, "Pacific/Auckland", 22, 0);
        snapshot(&mut h, if future { &[("2026-10-21", "0.03")] } else { &[] });
        hold(&mut h);
        let before = h.balance(acct(1));
        schedule(&mut h, "Europe/London", 22, 0);
        h.advance_to(h.engine.next_rollover().unwrap());
        let events = h.cmd(Command::Tick);
        let expected = if scalar { vec![charge(&h, "0.04")] } else { vec![] };
        assert_eq!(amounts(&events), expected, "scalar={scalar}, future={future}");
        assert_eq!(h.balance(acct(1)), before - expected.into_iter().sum::<Fixed>());
        assert!(!events.iter().any(|event| matches!(event,
            Event::BenchmarkRateObserved { benchmark, .. } if benchmark == "SOFR")));
        if scalar {
            assert!(events.contains(&Event::BenchmarkRateUndated { benchmark: "SOFR".into() }));
        } else {
            assert!(events.iter().any(|event| matches!(event,
                Event::BenchmarkRateMissing { benchmark, markets } if benchmark == "SOFR" && markets.contains(&BTC))));
        }
        assert_replay(&mut h);
    }
}

#[test]
fn hour_changes_within_the_same_timezone_keep_the_current_dated_rate() {
    for hour in [21, 23] {
        let mut h = fixture(false);
        snapshot(&mut h, &[("2026-10-19", "0.01"), ("2026-10-20", "0.02")]);
        hold(&mut h);
        let before = h.balance(acct(1));
        schedule(&mut h, "Europe/London", hour, 0);
        let next = h.engine.next_rollover().unwrap();
        assert_eq!(h.engine.benchmark_date(next), day("2026-10-20"));
        h.advance_to(next);
        let events = h.cmd(Command::Tick);
        assert_charge(&h, &events, "0.02", "2026-10-20", "2026-10-20");
        assert_eq!(h.balance(acct(1)), before - charge(&h, "0.02"));
        assert_eq!(history_days(&h), [day("2026-10-20")]);
        assert_replay(&mut h);
    }
}

#[test]
fn retention_discards_only_superseded_prefix_and_catch_up_keeps_each_overdue_rate() {
    let mut h = fixture(false);
    snapshot(
        &mut h,
        &[("2026-10-01", "0.009"), ("2026-10-17", "0.017"), ("2026-10-19", "0.019"), ("2026-10-20", "0.02")],
    );
    hold(&mut h);
    let before = h.balance(acct(1));
    let retained = history_days(&h);
    h.advance_to(stamp("2026-10-22T21:00:00.001Z"));
    let events = snapshot(&mut h, &[("2026-10-21", "0.021"), ("2026-10-22", "0.022")]);
    let expected = [charge(&h, "0.02"), charge(&h, "0.021"), charge(&h, "0.022")];
    assert_eq!(amounts(&events), expected);
    for (effective, rate) in [("2026-10-20", "0.02"), ("2026-10-21", "0.021"), ("2026-10-22", "0.022")] {
        assert!(
            events.contains(&Event::BenchmarkRateObserved {
                benchmark: "SOFR".into(),
                effective_date: day(effective),
                rollover_date: day(effective),
                stale: false,
            }),
            "rate {rate} must be used on {effective}"
        );
    }
    assert_eq!(h.balance(acct(1)), before - expected.into_iter().sum::<Fixed>());
    assert_eq!(retained, [day("2026-10-19"), day("2026-10-20")], "cap predecessor and newer date remain");
    assert_eq!(history_days(&h), [day("2026-10-22")]);
    assert!(amounts(&h.cmd(Command::Tick)).is_empty());
    assert_replay(&mut h);
}

#[test]
fn retention_accepts_the_earliest_and_latest_supported_engine_stamps() {
    for (at, observations, retained) in [
        (0, vec![("1970-01-01", "0.01")], "1970-01-01"),
        (engine_core::types::MAX_STAMP_MS, vec![("9999-12-30", "0.01"), ("9999-12-31", "0.02")], "9999-12-31"),
    ] {
        let mut h = fixture(false);
        let genesis = std::mem::take(&mut h.log);
        h.engine = Engine::new();
        h.ts = at;
        for mut command in genesis {
            assert!(!matches!(command.cmd, Command::SignedAdmin(_)));
            command.ts = at;
            h.engine.apply(&command).unwrap();
            h.log.push(command);
        }
        snapshot(&mut h, &observations);
        assert_eq!(history_days(&h), [day(retained)]);
        let mut replay = Engine::new();
        for command in &h.log {
            replay.apply(command).unwrap();
        }
        assert_eq!(replay.state_root(), h.engine.state_root());
    }
}

/// Starts with no legacy rate, or explicitly records a conflicting scalar before V18 activation.
fn fixture(scalar: bool) -> Harness {
    let mut h = Harness::v17();
    assert!(h.engine.benchmark_history().is_empty());
    assert_eq!(serde_json::to_value(&h.engine).unwrap()["benchmarks"], serde_json::json!({}));
    if scalar {
        h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 400 });
    }
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
    h
}

/// Changes only the signed rollover settings at the current sequenced time.
fn schedule(h: &mut Harness, tz: &str, hour: u32, minute: u32) {
    let mut params = h.engine.params().unwrap().clone();
    params.rollover = Rollover { tz: tz.parse().unwrap(), hour, minute };
    h.operator_admin(AdminCommand::SetParams(params));
}

/// Installs a complete operator-authenticated observation set and returns any catch-up events.
fn snapshot(h: &mut Harness, observations: &[(&str, &str)]) -> Vec<Event> {
    let batch = BenchmarkBatch {
        as_of: h.engine.benchmark_date(h.ts),
        checked: ["SOFR".to_owned()].into(),
        rates: observations
            .iter()
            .map(|(effective, rate)| BenchmarkRate {
                benchmark: "SOFR".into(),
                effective_date: day(effective),
                annual_rate: f(rate),
            })
            .collect(),
    };
    let events = h.operate(AdminCommand::SetBenchmarkRates(batch));
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    events
}

/// Opens exactly 100,000 USD of marked notional, with ample collateral to survive the charges.
fn hold(h: &mut Harness) {
    h.deposit(acct(1), "100000");
    h.price(BTC, "100000");
    let events = h.market_order(acct(1), BTC, Side::Buy, "1", "100000");
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
}

/// Computes the existing house-rounded financing for the held notional.
fn charge(h: &Harness, rate: &str) -> Fixed {
    let financing = &h.engine.markets()[&BTC].spec.financing;
    f("100000")
        .mul_rounded(f(rate) + Fixed::from_bps(financing.markup_bps), Rounding::Up)
        .div_int_rounded(financing.day_basis, Rounding::Up)
}

/// Checks financing before state shape, so the original defect fails its financial claim.
fn assert_charge(h: &Harness, events: &[Event], rate: &str, effective: &str, rollover: &str) {
    assert_eq!(amounts(events), [charge(h, rate)], "wrong effective rate charged at {rollover}");
    assert!(
        events.contains(&Event::BenchmarkRateObserved {
            benchmark: "SOFR".into(),
            effective_date: day(effective),
            rollover_date: day(rollover),
            stale: false,
        }),
        "{events:?}"
    );
    assert_eq!(h.engine.benchmark_history()["SOFR"][&day(effective)], f(rate));
}

/// Reads the BTC financing amounts in event order.
fn amounts(events: &[Event]) -> Vec<Fixed> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Financing { market: BTC, amount, .. } => Some(*amount),
            _ => None,
        })
        .collect()
}

/// Reads retained dates, preserving their protocol map order.
fn history_days(h: &Harness) -> Vec<NaiveDate> {
    h.engine.benchmark_history().get("SOFR").map(|history| history.keys().copied().collect()).unwrap_or_default()
}

/// Replays every signed command from genesis and verifies root, financing state, and conservation.
fn assert_replay(h: &mut Harness) {
    let mut replay = Engine::new();
    for command in &h.log {
        replay.apply(command).unwrap();
    }
    assert_eq!(replay.state_root(), h.engine.state_root());
    assert_eq!(replay.accounts()[&acct(1)].balance, h.balance(acct(1)));
    assert_eq!(replay.benchmark_history(), h.engine.benchmark_history());
    assert_eq!(replay.next_rollover(), h.engine.next_rollover());
    h.assert_conserved(Fixed::ZERO);
}

/// Parses an explicit fixture UTC timestamp without reading a clock.
fn stamp(value: &str) -> i64 {
    DateTime::parse_from_rfc3339(value).unwrap().timestamp_millis()
}

/// Parses an explicit publisher effective date.
fn day(value: &str) -> NaiveDate {
    value.parse().unwrap()
}
