//! Benchmark episode retirement follows actual accepted catalogue transitions.

use super::*;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine_core::command::{AdminCommand, Command, Sequenced};
use k256::ecdsa::SigningKey;
use tokio::sync::{broadcast, mpsc, watch};

use crate::admin::sign;
use crate::clock::test_clock::TestClock;
use crate::log::Manifest;
use crate::sequencer::Sequencer;
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, log_entries, sample_around, scratch_dir, sequencer_config};
use crate::trusted_time::tests::logs::Logs;

const START: i64 = 1_792_504_800_000;
const NAMES: [&str; 3] = ["benchmark_rate_missing", "benchmark_rate_undated", "benchmark_rate_stale"];

#[test]
fn benchmark_retirement_closes_each_uncleared_condition_and_reopens_after_relisting() {
    for (condition, name) in NAMES.iter().enumerate() {
        let (logs, _guard) = Logs::capture();
        let mut alerts = BenchmarkAlerts::default();
        let listed = catalogue(&["SOFR"]);
        let before = BenchmarkAlerts::listed(&listed);
        let event = firing("SOFR", condition);
        alerts.note(&event);
        alerts.retain_listed(&Engine::new(), &before);
        assert_eq!(clears(&logs), [1, 1, 1], "every firing or unknown condition closes on actual retirement");
        assert!(alerts.tracked.is_empty());
        alerts.retain_listed(&Engine::new(), &BTreeSet::new());
        assert_eq!(clears(&logs), [1, 1, 1], "repeated non-retirement adds no phantom clears");
        alerts.retain_listed(&listed, &BTreeSet::new());
        assert!(alerts.tracked.is_empty(), "relisting alone creates no local alert conditions");
        assert_eq!(clears(&logs), [1, 1, 1]);
        alerts.note(&event);
        assert_eq!(logs.critical(name), 2, "the returning benchmark can fire again");
        alerts.retain_listed(&Engine::new(), &before);
        assert_eq!(clears(&logs), [2, 2, 2]);
        assert!(alerts.tracked.is_empty());
    }
}

#[test]
fn benchmark_retirement_closes_multiple_firing_and_unknown_conditions_independently() {
    let (logs, _guard) = Logs::capture();
    let mut alerts = BenchmarkAlerts::default();
    alerts.note(&firing("SOFR", 0));
    alerts.note(&firing("SOFR", 1));
    alerts.note(&firing("SOFR", 0));
    assert_eq!(clears(&logs), [1, 0, 0], "the undated fallback already resolved missing");
    let before = BenchmarkAlerts::listed(&catalogue(&["SOFR"]));
    alerts.retain_listed(&Engine::new(), &before);
    assert_eq!(clears(&logs), [2, 1, 1], "retirement closes both reopened/firing conditions and unknown stale");
    assert!(alerts.tracked.is_empty());
}

#[test]
fn benchmark_retirement_does_not_duplicate_conditions_already_cleared() {
    let (logs, _guard) = Logs::capture();
    let mut alerts = BenchmarkAlerts::default();
    alerts.note(&observed("SOFR", false));
    assert_eq!(clears(&logs), [1, 1, 1]);
    let before = BenchmarkAlerts::listed(&catalogue(&["SOFR"]));
    alerts.retain_listed(&Engine::new(), &before);
    alerts.retain_listed(&Engine::new(), &BTreeSet::new());
    assert!(alerts.tracked.is_empty());
    assert_eq!(clears(&logs), [1, 1, 1]);
}

#[test]
fn benchmark_retirement_retains_active_names_until_their_last_market_changes() {
    let (logs, _guard) = Logs::capture();
    let mut alerts = BenchmarkAlerts::default();
    alerts.note(&firing("SOFR", 0));
    alerts.note(&firing("SONIA", 1));
    let both = catalogue(&["SOFR", "SOFR", "SONIA"]);
    let partial = catalogue(&["SOFR", "SONIA"]);
    alerts.retain_listed(&partial, &BenchmarkAlerts::listed(&both));
    assert_eq!(clears(&logs), [1, 0, 0], "partial retirement adds no clear beyond SONIA's fallback");
    assert_eq!(alerts.tracked.len(), 2);
    assert_eq!(alerts.tracked["SOFR"].missing, Some(false));
    assert_eq!(alerts.tracked["SONIA"].undated, Some(false));
    alerts.retain_listed(&catalogue(&["SONIA"]), &BenchmarkAlerts::listed(&partial));
    assert_eq!(clears(&logs), [2, 1, 1]);
    assert_eq!(alerts.tracked.len(), 1);
    assert_eq!(alerts.tracked["SONIA"].undated, Some(false), "other active episodes retain their state");
    alerts.retain_listed(&catalogue(&["SONIA"]), &BenchmarkAlerts::listed(&catalogue(&["SONIA"])));
    assert_eq!(clears(&logs), [2, 1, 1]);
}

#[test]
fn benchmark_retirement_after_restart_clears_unknown_old_episodes_without_a_due_rollover() {
    let (logs, _guard) = Logs::capture();
    let (mut seq, dir) = fixture("benchmark-retirement-restart");
    let first = seq.engine.next_rollover().unwrap();
    seq.stamp(first + 1, Command::Tick).unwrap();
    assert_eq!(logs.critical(NAMES[0]), 1);
    assert_eq!(clears(&logs), [0, 0, 0]);
    seq.flush().unwrap();
    drop(seq);
    let mut seq = recover_at(&dir, first + 1);
    assert!(seq.benchmark_alerts.tracked.is_empty(), "local tracker is unknown after real log recovery");
    let next = seq.engine.next_rollover();
    let mut market = seq.engine.markets()[&1].spec.clone();
    market.financing.benchmark = "SONIA".into();
    let mut invalid = market.clone();
    invalid.margin_tiers.clear();
    let events = apply_admin(&mut seq, first + 1, AdminCommand::UpsertMarket(invalid));
    assert!(events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    assert_eq!(seq.engine.markets()[&1].spec.financing.benchmark, "SOFR");
    assert_eq!(clears(&logs), [0, 0, 0], "rejected change leaves the external old episode open");

    let events = apply_admin(&mut seq, first + 1, AdminCommand::UpsertMarket(market.clone()));
    assert!(events.contains(&Event::AdminApplied));
    assert!(!events.iter().any(|event| matches!(event, Event::BenchmarkRateMissing { .. })));
    assert_eq!(seq.engine.next_rollover(), next, "retirement did not need a new rollover event");
    assert_eq!(clears(&logs), [1, 1, 1], "unknown old-name conditions resolve after restart");
    assert!(seq.benchmark_alerts.tracked.is_empty());
    apply_admin(&mut seq, first + 1, AdminCommand::UpsertMarket(market.clone()));
    assert_eq!(clears(&logs), [1, 1, 1], "unchanged catalogue produces no phantom retirement");
    market.financing.benchmark = "SOFR".into();
    apply_admin(&mut seq, first + 1, AdminCommand::UpsertMarket(market));
    assert_eq!(clears(&logs), [2, 2, 2], "actual unknown SONIA retirement also resolves once");
    seq.stamp(next.unwrap() + 1, Command::Tick).unwrap();
    assert_eq!(logs.critical(NAMES[0]), 2, "returned SOFR fires a new episode");
    assert_eq!(clears(&logs), [2, 2, 2]);
    assert_eq!(seq.benchmark_alerts.tracked["SOFR"].missing, Some(false));
}

#[test]
fn benchmark_retirement_follows_old_rollover_events_and_rejected_admin_keeps_firing() {
    let (logs, _guard) = Logs::capture();
    let (mut seq, _) = fixture("benchmark-retirement-event-order");
    let first = seq.engine.next_rollover().unwrap();
    seq.stamp(first + 1, Command::Tick).unwrap();
    let mut market = seq.engine.markets()[&1].spec.clone();
    market.financing.benchmark = "SONIA".into();
    let mut invalid = market.clone();
    invalid.margin_tiers.clear();
    let second = seq.engine.next_rollover().unwrap();
    let events = apply_admin(&mut seq, second + 1, AdminCommand::UpsertMarket(invalid));
    assert!(events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    assert_eq!(seq.benchmark_alerts.tracked["SOFR"].missing, Some(false));
    assert_eq!(clears(&logs), [0, 0, 0]);
    let third = seq.engine.next_rollover().unwrap();
    let events = apply_admin(&mut seq, third + 1, AdminCommand::UpsertMarket(market.clone()));
    assert!(events.contains(&Event::AdminApplied));
    assert!(events.iter().any(|event| matches!(event,
        Event::BenchmarkRateMissing { benchmark, .. } if benchmark == "SOFR")));
    assert_eq!(logs.critical(NAMES[0]), 3, "old-name rollover fires before accepted retirement");
    assert_eq!(clears(&logs), [1, 1, 1]);
    assert!(seq.benchmark_alerts.tracked.is_empty(), "old rollover events cannot reinsert retired names");
    apply_admin(&mut seq, third + 1, AdminCommand::UpsertMarket(market));
    assert_eq!(clears(&logs), [1, 1, 1]);
}

fn firing(benchmark: &str, condition: usize) -> Event {
    match condition {
        0 => Event::BenchmarkRateMissing { benchmark: benchmark.into(), markets: vec![1] },
        1 => Event::BenchmarkRateUndated { benchmark: benchmark.into() },
        2 => observed(benchmark, true),
        _ => unreachable!("three benchmark conditions"),
    }
}

fn observed(benchmark: &str, stale: bool) -> Event {
    Event::BenchmarkRateObserved {
        benchmark: benchmark.into(),
        effective_date: if stale { "2026-10-20" } else { "2026-10-28" }.parse().unwrap(),
        rollover_date: "2026-10-28".parse().unwrap(),
        stale,
    }
}

fn clears(logs: &Logs) -> [usize; 3] {
    NAMES.map(|name| logs.count(&format!("alert_resolved=\"{name}\"")))
}

fn catalogue(benchmarks: &[&str]) -> Engine {
    let mut engine = Engine::new();
    let template = dev_config().genesis.markets[0].clone();
    for (index, benchmark) in benchmarks.iter().enumerate() {
        let mut market = template.clone();
        market.id = u32::try_from(index).unwrap() + 1;
        market.symbol = format!("retirement-{index}");
        market.financing.benchmark = (*benchmark).into();
        let events = engine
            .apply(&Sequenced { seq: index as u64, ts: 0, cmd: Command::Admin(AdminCommand::UpsertMarket(market)) })
            .unwrap();
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
    }
    engine
}

fn fixture(name: &str) -> (Sequencer, PathBuf) {
    let dir = scratch_dir(name);
    let mut seq = recover_at(&dir, START);
    seq.trust(sample_around(seq.clock.as_ref())).unwrap();
    seq.activate_protocol().unwrap();
    let mut genesis = dev_config().genesis;
    genesis.markets.retain(|market| market.id == 1);
    genesis.close_spread_ceilings.retain(|ceiling| ceiling.market == 1);
    genesis.benchmarks.clear();
    seq.genesis(&genesis).unwrap();
    (seq, dir)
}

fn recover_at(dir: &Path, at: i64) -> Sequencer {
    let clock = Arc::new(TestClock::at(at));
    let genesis = crate::sequencer::fixture_tests::finished_genesis(&log_entries(dir));
    let manifest = watch::channel(Manifest::default()).0;
    let cfg = crate::sequencer::SequencerConfig { clock, genesis, ..sequencer_config(dir, manifest) };
    Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(16).0, mpsc::unbounded_channel().0)
        .unwrap()
}

fn apply_admin(seq: &mut Sequencer, at: i64, command: AdminCommand) -> Vec<Event> {
    let key = SigningKey::from_bytes((&[0x42; 32]).into()).unwrap();
    let request = sign(&key, &seq.operator, command, seq.engine.next_request_nonce(seq.operator.address), at + 60_000);
    seq.stamp(at, Command::SignedAdmin(Box::new(request))).unwrap()
}
