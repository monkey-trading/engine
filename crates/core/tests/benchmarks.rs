//! Dated financing and replay at the V18 protocol boundary: each missed rollover's rate in catch-up,
//! signed snapshots and their bounds, missing, stale and undated rates, and the rollover's local date.

mod common;

use chrono::NaiveDate;
use common::{BTC, Harness, acct, f};
use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate, business_days};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::fixed::Rounding;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;
use engine_core::{Engine, Fixed};

fn date(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

fn batch(h: &Harness, observations: &[(&str, &str)]) -> AdminCommand {
    AdminCommand::SetBenchmarkRates(BenchmarkBatch {
        as_of: h.engine.benchmark_date(h.ts),
        checked: ["SOFR".to_owned()].into(),
        rates: observations
            .iter()
            .map(|(day, rate)| BenchmarkRate {
                benchmark: "SOFR".into(),
                effective_date: date(day),
                annual_rate: f(rate),
            })
            .collect(),
    })
}

fn dated() -> Harness {
    let mut h = Harness::v17();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
    h
}

fn hold(h: &mut Harness) {
    h.deposit(acct(1), "100000");
    h.price(BTC, "100000");
    let fills = h.market_order(acct(1), BTC, Side::Buy, "1", "100000");
    assert!(fills.iter().any(|e| matches!(e, Event::Fill(_))), "{fills:?}");
}

fn expected(h: &Harness, rate: &str) -> Fixed {
    let financing = &h.engine.markets()[&BTC].spec.financing;
    f("100000")
        .mul_rounded(f(rate) + Fixed::from_bps(financing.markup_bps), Rounding::Up)
        .div_int_rounded(financing.day_basis, Rounding::Up)
}

fn amounts(events: &[Event]) -> Vec<Fixed> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Financing { market: BTC, amount, .. } => Some(*amount),
            _ => None,
        })
        .collect()
}

#[test]
fn each_missed_rollover_uses_its_effective_rate_before_catch_up_and_replays() {
    let mut h = dated();
    let first = h.engine.next_rollover().unwrap();
    h.operator_admin(batch(&h, &[("2026-10-20", "0.01")]));
    hold(&mut h);
    h.advance_to(first + 1);
    let events = h.cmd(Command::Tick);
    assert_eq!(amounts(&events), [expected(&h, "0.01")]);
    let before = h.balance(acct(1));

    // No command is sequenced during two rollovers. The restart snapshot installs the rate
    // change effective on the latter before catch-up can debit either missed day.
    h.advance_to(first + 2 * 86_400_000 + 1);
    let events = h.operate(batch(&h, &[("2026-10-21", "0.015"), ("2026-10-22", "0.02003")]));
    assert!(events.contains(&Event::AdminApplied));
    assert_eq!(amounts(&events), [expected(&h, "0.015"), expected(&h, "0.02003")]);
    assert_eq!(h.balance(acct(1)), before - expected(&h, "0.015") - expected(&h, "0.02003"));
    assert!(amounts(&h.cmd(Command::Tick)).is_empty(), "catch-up must apply once");
    assert_eq!(h.engine.benchmark_history()["SOFR"].len(), 1, "only the next rollover's predecessor remains");
    h.assert_conserved(Fixed::ZERO);
    let root = h.engine.state_root();
    let mut replay = Engine::new();
    for cmd in &h.log {
        replay.apply(cmd).unwrap();
    }
    assert_eq!(replay.state_root(), root);
}

#[test]
fn a_stale_rate_still_charges_and_the_sixth_weekday_raises_the_status() {
    let mut h = dated();
    h.operator_admin(batch(&h, &[("2026-10-20", "-0.00123")]));
    hold(&mut h);
    let first = h.engine.next_rollover().unwrap();
    h.advance_to(first + 8 * 86_400_000 + 7_200_000);
    let events = h.operate(batch(&h, &[]));
    assert_eq!(amounts(&events), vec![expected(&h, "-0.00123"); 9]);
    let statuses: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::BenchmarkRateObserved { rollover_date, stale, .. } => Some((*rollover_date, *stale)),
            _ => None,
        })
        .collect();
    assert!(statuses.contains(&(date("2026-10-27"), false)), "five weekdays is still current");
    assert!(statuses.contains(&(date("2026-10-28"), true)), "the sixth weekday is stale");
    assert_eq!(business_days(date("2026-10-23"), date("2026-10-25")), 0);
    assert_eq!(business_days(date("2026-10-23"), date("2026-10-26")), 1);
}

#[test]
fn a_missing_dated_rate_skips_financing_and_names_the_unrated_markets() {
    let mut h = dated();
    // The production genesis leaves benchmark rates unset. Rebuild that serializable state.
    let mut state = serde_json::to_value(&h.engine).unwrap();
    state["benchmarks"] = serde_json::json!({});
    state["benchmark_history"] = serde_json::json!({});
    h.engine = serde_json::from_value(state).unwrap();
    hold(&mut h);
    h.advance_to(h.engine.next_rollover().unwrap() + 1);
    let before = h.balance(acct(1));
    let events = h.operate(batch(&h, &[]));
    assert!(amounts(&events).is_empty());
    assert_eq!(h.balance(acct(1)), before);
    assert!(events.iter().any(|e| matches!(e, Event::BenchmarkRateMissing { benchmark, markets } if benchmark == "SOFR" && markets.contains(&BTC))));
}

#[test]
fn invalid_or_unsigned_snapshots_cannot_install_rates_or_charge_pending_rollovers() {
    let mut h = dated();
    hold(&mut h);
    h.advance_to(h.engine.next_rollover().unwrap() + 1);
    let history = h.engine.benchmark_history().clone();
    let next = h.engine.next_rollover();
    let valid = batch(&h, &[("2026-10-20", "0.09")]);
    let unsigned = h.cmd(Command::Admin(valid.clone()));
    assert!(unsigned.iter().any(|e| matches!(e, Event::AdminRejected { .. })));
    assert!(amounts(&unsigned).is_empty());
    for change in 0..11 {
        let AdminCommand::SetBenchmarkRates(mut b) = valid.clone() else { unreachable!() };
        match change {
            0 => b.rates[0].annual_rate = f("1.000000001"),
            1 => b.rates[0].effective_date = date("2026-10-21"),
            2 => b.rates.push(b.rates[0].clone()),
            3 => b.checked.clear(),
            4 => b.as_of = date("2026-10-19"),
            5 => b.rates[0].benchmark = "unknown".into(),
            6 => b.rates[0].annual_rate = f("-1.000000001"),
            7 => b.rates[0].effective_date = date("1969-12-31"),
            8 => {
                b.rates[0].benchmark.clear();
                b.checked.insert(String::new());
            }
            9 => {
                b.rates[0].benchmark = "x".repeat(65);
                b.checked.insert(b.rates[0].benchmark.clone());
            }
            10 => b.checked.extend((0..256).map(|i| format!("extra-{i}"))),
            _ => unreachable!(),
        }
        // A later invalid observation must not install earlier valid data.
        b.rates.insert(
            0,
            BenchmarkRate { benchmark: "SOFR".into(), effective_date: date("2026-10-19"), annual_rate: f("0.04") },
        );
        let events = h.operate(AdminCommand::SetBenchmarkRates(b));
        assert!(events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "case {change}: {events:?}");
        assert!(amounts(&events).is_empty());
        assert_eq!(h.engine.benchmark_history(), &history);
        assert_eq!(h.engine.next_rollover(), next);
    }
    let events = h.operate(valid);
    assert_eq!(amounts(&events), [expected(&h, "0.09")]);
}

#[test]
fn versions_before_v18_refuse_dated_rates_and_retain_legacy_roots() {
    let mut h = Harness::v17();
    let events = h.operate(batch(&h, &[("2026-10-20", "0.01")]));
    assert!(events.iter().any(|e| matches!(e, Event::AdminRejected { reason } if reason.contains("V18"))));
    assert!(h.engine.benchmark_history().is_empty());
    assert!(serde_json::to_value(&h.engine).unwrap().get("benchmark_history").is_none());
}

#[test]
fn rollover_dates_follow_the_protocol_timezone_across_utc_midnight() {
    let mut h = dated();
    let mut params = h.engine.params().unwrap().clone();
    params.rollover.tz = chrono_tz::America::New_York;
    h.operator_admin(AdminCommand::SetParams(params));
    // With rollover in New York: UTC midnight is still the previous calendar date.
    let ts = chrono::DateTime::parse_from_rfc3339("2026-10-22T00:30:00Z").unwrap().timestamp_millis();
    assert_eq!(h.engine.benchmark_date(ts), date("2026-10-21"));
}

#[test]
fn legacy_rates_with_unknown_dates_still_apply_and_raise_an_undated_status() {
    let mut h = Harness::v17();
    h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 400 });
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
    assert!(h.engine.benchmark_history().is_empty(), "activation must not invent publisher dates");
    hold(&mut h);
    h.advance_to(h.engine.next_rollover().unwrap() + 1);
    let events = h.operate(batch(&h, &[]));
    assert_eq!(amounts(&events), [expected(&h, "0.04")]);
    assert!(events.contains(&Event::BenchmarkRateUndated { benchmark: "SOFR".into() }));
}

#[test]
fn snapshot_rate_bounds_and_size_limits_are_checked_before_any_install() {
    use engine_core::benchmark::MAX_BENCHMARK_RATES;
    let mut h = dated();
    for rate in ["0", "1", "-1"] {
        let cmd = batch(&h, &[("2026-10-20", rate)]);
        h.operator_admin(cmd);
        assert_eq!(h.engine.benchmark_history()["SOFR"][&date("2026-10-20")], f(rate));
    }
    let AdminCommand::SetBenchmarkRates(mut too_many) = batch(&h, &[("2026-10-20", "0.01")]) else { unreachable!() };
    too_many.rates = vec![too_many.rates[0].clone(); MAX_BENCHMARK_RATES + 1];
    let before = h.engine.benchmark_history().clone();
    let events = h.operate(AdminCommand::SetBenchmarkRates(too_many));
    assert!(events.iter().any(|e| matches!(e, Event::AdminRejected { reason } if reason.contains("too large"))));
    assert_eq!(h.engine.benchmark_history(), &before);
}

#[test]
fn active_snapshots_keep_future_calendar_predecessors_until_they_are_superseded() {
    let mut h = dated();
    h.operator_admin(batch(&h, &[("2026-10-19", "0.01"), ("2026-10-20", "0.02")]));
    assert_eq!(h.engine.benchmark_history()["SOFR"].len(), 2);
    let boundary = chrono::DateTime::parse_from_rfc3339("2026-10-20T18:00:00Z").unwrap().timestamp_millis();
    h.advance_to(boundary - 1);
    h.operator_admin(batch(&h, &[]));
    assert_eq!(h.engine.benchmark_history()["SOFR"].len(), 2);
    h.advance_to(boundary);
    h.operator_admin(batch(&h, &[]));
    let retained = &h.engine.benchmark_history()["SOFR"];
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[&date("2026-10-20")], f("0.02"));
}

#[test]
fn a_short_receives_the_dated_benchmark_less_markup_rounded_toward_the_house() {
    let mut h = dated();
    h.deposit(acct(1), "100000");
    h.price(BTC, "100000");
    let fills = h.market_order(acct(1), BTC, Side::Sell, "1", "100000");
    assert!(fills.iter().any(|e| matches!(e, Event::Fill(_))), "{fills:?}");
    h.advance_to(h.engine.next_rollover().unwrap() + 1);
    let before = h.balance(acct(1));
    let events = h.operate(batch(&h, &[("2026-10-20", "0.09")]));
    // (2.5% - 9%) * 100,000 / 365 = -17.808219178082..., rounded up.
    assert_eq!(amounts(&events), [f("-17.808219178")]);
    assert_eq!(h.balance(acct(1)), before + f("17.808219178"));
    h.assert_conserved(Fixed::ZERO);
}
