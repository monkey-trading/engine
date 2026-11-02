//! The cap on signed benchmark rates from `V46`, through `Engine::apply`: a move from an applied
//! rate limited per day between their dates, a benchmark's first rate bound only by its range, and
//! what a refused rate leaves in force for financing.

mod common;

use chrono::{Duration, NaiveDate};
use common::close_spread::{attest, ceilings, fund, install_expect, source_key, trade};
use common::{BTC, Client, Harness, f};
use engine_core::Fixed;
use engine_core::access::AccessPolicy;
use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate, split_by_cap};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};
use proptest::collection::vec;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

const DAY_MS: Millis = 24 * 60 * 60 * 1000;
/// The seed of the client whose position pays financing on each rollover, for the financing tests.
const HOLDER: u8 = 71;

/// `version` over the fixture's markets, with a client funded through screening.
fn at(version: ProtocolVersion) -> Harness {
    let mut h = Harness::v21();
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    h.admin(AdminCommand::ActivateProtocol { version });
    let holder = Client::new(HOLDER);
    attest(&mut h, &holder);
    fund(&mut h, &holder, "100000");
    h
}

fn capped() -> Harness {
    at(ProtocolVersion::V46)
}

/// `days` before the rollover-local date now.
fn ago(h: &Harness, days: i64) -> NaiveDate {
    h.engine.benchmark_date(h.ts) - Duration::days(days)
}

/// A snapshot of `observations` for SOFR, as `(days ago, annual rate)`.
fn snapshot(h: &Harness, observations: &[(i64, &str)]) -> AdminCommand {
    AdminCommand::SetBenchmarkRates(BenchmarkBatch {
        as_of: h.engine.benchmark_date(h.ts),
        checked: ["SOFR".to_owned()].into(),
        rates: observations.iter().map(|(days, rate)| sofr(ago(h, *days), rate)).collect(),
    })
}

fn sofr(effective_date: NaiveDate, rate: &str) -> BenchmarkRate {
    BenchmarkRate { benchmark: "SOFR".into(), effective_date, annual_rate: f(rate) }
}

/// The rate history holds for SOFR on a date.
fn applied(h: &Harness, date: NaiveDate) -> Option<Fixed> {
    h.engine.benchmark_history().get("SOFR").and_then(|history| history.get(&date)).copied()
}

/// Whether signing `observations` applied, not leaving the history changed when it is refused.
fn signs(h: &mut Harness, observations: &[(i64, &str)]) -> bool {
    let before = h.engine.benchmark_history().clone();
    let command = snapshot(h, observations);
    let events = h.operate(command);
    let done = events.contains(&Event::AdminApplied);
    if !done {
        assert_eq!(h.engine.benchmark_history(), &before, "a refused snapshot changes nothing: {events:?}");
        assert!(matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("bps a day")));
    }
    done
}

#[test]
fn a_move_of_the_cap_applies_and_one_unit_past_it_is_refused_either_way() {
    for (limit, past) in [("0.07", "0.070000001"), ("0.01", "0.009999999")] {
        let mut h = capped();
        assert!(signs(&mut h, &[(2, "0.04")]));
        assert!(!signs(&mut h, &[(1, past)]), "{past}");
        assert_eq!(applied(&h, ago(&h, 1)), None);
        assert!(signs(&mut h, &[(1, limit)]), "{limit}");
        assert_eq!(applied(&h, ago(&h, 1)), Some(f(limit)));
    }
}

#[test]
fn a_gap_between_dates_scales_the_limit_by_the_days_between_them() {
    for (days, limit, past) in [(3, "0.13", "0.130000001"), (7, "0.25", "0.250000001"), (30, "0.94", "0.940000001")] {
        let mut h = capped();
        assert!(signs(&mut h, &[(days, "0.04")]));
        assert!(!signs(&mut h, &[(0, past)]), "{days} days");
        assert!(signs(&mut h, &[(0, limit)]), "{days} days");
    }
}

#[test]
fn a_benchmarks_first_rate_is_bound_only_by_its_range() {
    for rate in ["1", "-1", "0.5"] {
        let mut h = capped();
        assert!(signs(&mut h, &[(0, rate)]), "{rate}");
        assert_eq!(applied(&h, ago(&h, 0)), Some(f(rate)));
    }
    let mut h = capped();
    let command = snapshot(&h, &[(0, "1.000000001")]);
    assert_eq!(
        h.operate(command).last(),
        Some(&Event::AdminRejected { reason: "benchmark annual rate must be within +/-100%".into() })
    );
}

#[test]
fn a_snapshot_is_refused_whole_for_one_rate_past_the_cap() {
    let mut h = capped();
    assert!(signs(&mut h, &[(2, "0.04")]));
    // The first date is fine and the second is not: neither applies.
    assert!(!signs(&mut h, &[(1, "0.05"), (0, "0.2")]));
    assert_eq!(applied(&h, ago(&h, 1)), None);
    // The rates are measured in date order however the snapshot lists them.
    assert!(signs(&mut h, &[(0, "0.1"), (1, "0.07")]));
    assert_eq!(applied(&h, ago(&h, 0)), Some(f("0.1")));
}

#[test]
fn sending_an_applied_rate_again_passes_whatever_is_next_to_it() {
    // Applied before the cap: the move between them is past it, and the worker sends the predecessor
    // with each snapshot. Both stay in the history, the older as the rate in force a day ago.
    let mut h = at(ProtocolVersion::V45);
    let first = snapshot(&h, &[(1, "0.04"), (0, "0.5")]);
    assert!(h.operate(first).contains(&Event::AdminApplied));
    assert_eq!((applied(&h, ago(&h, 1)), applied(&h, ago(&h, 0))), (Some(f("0.04")), Some(f("0.5"))));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V46 });
    assert!(signs(&mut h, &[(1, "0.04"), (0, "0.5")]));
}

#[test]
fn an_applied_date_does_not_change() {
    let mut h = capped();
    assert!(signs(&mut h, &[(1, "0.04")]));
    assert!(signs(&mut h, &[(0, "0.05")]));
    // By any amount, with or without a date before it in the history.
    assert!(!signs(&mut h, &[(0, "0.0501")]));
    assert!(!signs(&mut h, &[(1, "0.0401")]));
    assert_eq!(applied(&h, ago(&h, 0)), Some(f("0.05")));
    assert_eq!(applied(&h, ago(&h, 1)), Some(f("0.04")));
}

#[test]
fn a_backdated_rate_cannot_walk_the_rate_in_force_past_the_cap() {
    let mut h = capped();
    assert!(signs(&mut h, &[(0, "0.04")]));
    // Thirty days before it, the limit lets a far rate in, and it applies to no rollover to come.
    assert!(signs(&mut h, &[(30, "0.9")]));
    // Measured from that rate over the same gap a new value would pass: the date does not change.
    assert!(!signs(&mut h, &[(0, "0.9")]));
    assert_eq!(applied(&h, ago(&h, 0)), Some(f("0.04")));
}

#[test]
fn a_rate_between_two_dates_is_measured_from_each_of_them() {
    let mut h = capped();
    assert!(signs(&mut h, &[(5, "0.04")]));
    assert!(signs(&mut h, &[(0, "0.05")]));
    // Within four days' limit of the earlier, one day's limit of the later is the bound.
    assert!(!signs(&mut h, &[(1, "0.0801")]));
    assert!(signs(&mut h, &[(1, "0.08")]));
    assert_eq!(applied(&h, ago(&h, 1)), Some(f("0.08")));
    // Before every date, only the later one bounds it: two days' limit of 8%.
    assert!(!signs(&mut h, &[(3, "0.1401")]));
    assert!(signs(&mut h, &[(3, "0.14")]));
}

#[test]
fn the_cap_starts_at_v46_and_earlier_rules_take_any_rate_in_range() {
    let mut h = at(ProtocolVersion::V45);
    let first = snapshot(&h, &[(1, "0.04")]);
    assert!(h.operate(first).contains(&Event::AdminApplied));
    let second = snapshot(&h, &[(0, "0.9")]);
    assert!(h.operate(second).contains(&Event::AdminApplied));
    assert_eq!(applied(&h, ago(&h, 0)), Some(f("0.9")));
}

#[test]
fn a_signed_undated_rate_is_held_to_the_cap_too() {
    let set = |annual_bps| AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps };
    let refused = |events: Vec<Event>| matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("bps a day"));

    let mut h = capped();
    // The first is bound only by the range, and dated today.
    assert_eq!(h.operate(set(9_000)).last(), Some(&Event::AdminApplied));
    assert_eq!(applied(&h, ago(&h, 0)), Some(f("0.9")));
    // Signed again today it is the same date, which does not change, and the same rate passes.
    assert!(refused(h.operate(set(8_700))));
    assert_eq!(h.operate(set(9_000)).last(), Some(&Event::AdminApplied));
    // The next day it moves within a day's limit of the last date, and not past it.
    h.advance(DAY_MS);
    assert!(refused(h.operate(set(5_000))));
    assert_eq!(h.operate(set(8_700)).last(), Some(&Event::AdminApplied));
    assert_eq!(applied(&h, ago(&h, 0)), Some(f("0.87")));
}

#[test]
fn a_first_dated_rate_is_measured_a_day_from_an_undated_one() {
    let mut h = Harness::v17();
    h.operate(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 400 });
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V46 });
    assert!(!signs(&mut h, &[(0, "0.070000001")]));
    assert!(signs(&mut h, &[(0, "0.07")]));
}

/// Opens a long BTC position that pays financing each rollover.
fn hold(h: &mut Harness) {
    h.price(BTC, "100000");
    let events = trade(h, &Client::new(HOLDER), BTC, Side::Buy, "0.5", "100000");
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
}

/// The BTC financing in `events`, and the observation date each rollover used.
fn financed(events: &[Event]) -> (Vec<Fixed>, Vec<(NaiveDate, bool)>) {
    let amounts = events
        .iter()
        .filter_map(|event| match event {
            Event::Financing { market: BTC, amount, .. } => Some(*amount),
            _ => None,
        })
        .collect();
    let observed = events
        .iter()
        .filter_map(|event| match event {
            Event::BenchmarkRateObserved { effective_date, stale, .. } => Some((*effective_date, *stale)),
            _ => None,
        })
        .collect();
    (amounts, observed)
}

#[test]
fn a_refused_rate_leaves_the_last_applied_rate_financing_until_the_limit_reaches_a_real_move() {
    let mut h = capped();
    assert!(signs(&mut h, &[(0, "0.04")]));
    let start = h.engine.benchmark_date(h.ts);
    hold(&mut h);

    // The rate jumps 10% and stays there. One, two and three days' limits are short of it, so each is
    // refused, and each rollover meanwhile charges at the applied rate and observes its date.
    let mut charged = Vec::new();
    for day in 0..4 {
        let rollover = h.engine.next_rollover().unwrap();
        h.advance_to(rollover - 1);
        if day > 0 {
            assert!(!signs(&mut h, &[(0, "0.14")]), "day {day}");
        }
        h.advance_to(rollover + 1);
        let (amounts, observed) = financed(&h.cmd(Command::Tick));
        assert_eq!(observed, [(start, false)], "day {day}");
        charged.extend(amounts);
    }
    assert_eq!(charged.len(), 4);
    assert!(charged.windows(2).all(|pair| pair[0] == pair[1]), "{charged:?}");

    // On the fourth day the limit from the last applied date reaches the move, and it applies.
    let rollover = h.engine.next_rollover().unwrap();
    h.advance_to(rollover - 1);
    assert!(signs(&mut h, &[(0, "0.14")]));
    let moved = h.engine.benchmark_date(rollover - 1);
    h.advance_to(rollover + 1);
    let (amounts, observed) = financed(&h.cmd(Command::Tick));
    assert_eq!(observed, [(moved, false)]);
    assert!(amounts[0] > charged[0], "{amounts:?} {charged:?}");
}

#[test]
fn a_rate_refused_for_long_enough_turns_the_observation_stale_without_stopping_financing() {
    let mut h = capped();
    assert!(signs(&mut h, &[(0, "0.04")]));
    hold(&mut h);
    let (mut charged, mut stale) = (Vec::new(), Vec::new());
    for _ in 0..10 {
        let rollover = h.engine.next_rollover().unwrap();
        h.advance_to(rollover + 1);
        let (amounts, observed) = financed(&h.cmd(Command::Tick));
        charged.extend(amounts);
        stale.extend(observed.iter().map(|day| day.1));
    }
    assert_eq!(charged.len(), 10);
    assert!(charged.windows(2).all(|pair| pair[0] == pair[1]), "{charged:?}");
    assert_eq!((stale.first(), stale.last()), (Some(&false), Some(&true)), "{stale:?}");
}

#[test]
fn a_replay_reaches_the_same_root_over_a_refusal() {
    let mut h = capped();
    assert!(signs(&mut h, &[(2, "0.04")]));
    assert!(!signs(&mut h, &[(1, "0.5")]));
    assert!(signs(&mut h, &[(1, "0.05")]));
    let root = h.engine.state_root();
    assert_eq!(common::batch::replayed(&h).state_root(), root);
}

#[test]
fn what_the_cap_admits_is_what_the_engine_applies_and_every_applied_move_is_within_it() {
    let config =
        Config { cases: 128, rng_seed: RngSeed::Fixed(0x5eed_4635), failure_persistence: None, ..Config::default() };
    // Up to ten observations over the last twelve days, of up to ±30%, in any order.
    let strategy = vec((0i64..12, -300_000_000i128..=300_000_000i128), 1..10);
    let result = TestRunner::new(config).run(&strategy, |observations| {
        let mut h = capped();
        let today = h.engine.benchmark_date(h.ts);
        let mut rates: Vec<BenchmarkRate> = Vec::new();
        for (days, raw) in observations {
            let effective_date = today - Duration::days(days);
            if rates.iter().all(|rate| rate.effective_date != effective_date) {
                rates.push(BenchmarkRate {
                    benchmark: "SOFR".into(),
                    effective_date,
                    annual_rate: Fixed::from_raw(raw),
                });
            }
        }
        let (admitted, refused) = split_by_cap(h.engine.benchmark_history(), &Default::default(), &rates);
        let check = |holds: bool, what: &str| {
            if holds { Ok(()) } else { Err(TestCaseError::fail(format!("{what}: {rates:?}"))) }
        };
        check(admitted.len() + refused.len() == rates.len(), "every rate is admitted or refused")?;
        let command = AdminCommand::SetBenchmarkRates(BenchmarkBatch {
            as_of: today,
            checked: ["SOFR".to_owned()].into(),
            rates: admitted.clone(),
        });
        check(h.operate(command).contains(&Event::AdminApplied), "what the cap admits applies")?;
        // The history keeps what rollovers can still use, all of it among what was admitted.
        let history = h.engine.benchmark_history().get("SOFR").cloned().unwrap_or_default();
        let mut days: Vec<(NaiveDate, Fixed)> =
            admitted.iter().map(|rate| (rate.effective_date, rate.annual_rate)).collect();
        days.sort_by_key(|day| day.0);
        check(history.iter().all(|(day, rate)| days.contains(&(*day, *rate))), "what applied was admitted")?;
        for pair in days.windows(2) {
            let limit = Fixed::from_bps(300 * (pair[1].0 - pair[0].0).num_days());
            check((pair[1].1 - pair[0].1).abs() <= limit, "an admitted move is past the cap")?;
        }
        Ok(())
    });
    if let Err(error) = result {
        panic!("{error}");
    }
}
