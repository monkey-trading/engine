use super::*;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::{iter, net};

use crate::alerts::{self, AlertLayer, Routes};
use crate::config::HistorySection;
use crate::history::INDEX_FILE;
use crate::log::{LogEntry, MANIFEST, Offset, segment_seq};
use crate::test_support::{
    RETENTION, TestHistory, TestSequencer, capture_alerts, deposit, dev_config, log_entries, open_history, scratch_dir,
};

/// Startup's one read of the log decides the history index's fate: an index that took the log's
/// last command resumes after it, with no alert and no rebuild.
#[tokio::test]
async fn startup_resumes_a_history_index_its_read_of_the_log_matches() {
    let (dir, index) = (scratch_dir("node-history-resumes"), scratch_dir("node-history-resumes-index"));
    let node = TestSequencer::start_legacy(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    node.submit(deposit(1, "dep-1")).await;
    node.stop().await;
    history.finish().await.unwrap();
    let mut commands = log_entries(&dir).into_iter().filter_map(|entry| match entry {
        LogEntry::Command(command) => Some(command.sequenced.seq),
        _ => None,
    });
    let last = commands.next_back().expect("the log holds commands");

    let mut cfg = dev_config();
    cfg.alerts.outbox.dir = scratch_dir(&format!("node-outbox-{}", rand::random::<u64>()));
    (cfg.node.log_dir, cfg.history.dir, cfg.history.retention) = (dir, index, RETENTION);
    let (_guard, mut alerts) = capture_alerts();
    let (_, history) = read_log_and_history(&cfg).unwrap();
    assert_eq!(history.resume_at(), Offset::start_of(last + 1));
    assert!(alerts.try_recv().is_err(), "no alert");
}

/// A power loss that keeps what the history index took but not the log's unsynced tail leaves
/// the index ahead of the log. The node discards the index with a Warning alert and goes on to
/// start: here as far as binding the API, whose port the test holds.
#[tokio::test]
async fn a_history_index_ahead_of_its_log_is_discarded_and_the_node_starts() {
    let (kept, index) = index_ahead_of_log("node-history-ahead").await;
    let held = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg = dev_config();
    cfg.alerts.outbox.dir = scratch_dir(&format!("node-outbox-{}", rand::random::<u64>()));
    (cfg.node.log_dir, cfg.history.dir) = (kept.clone(), index.clone());
    cfg.node.listen = held.local_addr().unwrap();
    cfg.metrics.listen = "127.0.0.1:0".parse().unwrap();
    let (_guard, mut alerts) = capture_alerts();
    let (telemetry, _layer) = telemetry();
    let stopped = timeout(Duration::from_secs(10), run(cfg, telemetry)).await.expect("the node stops in time");
    let err = stopped.expect_err("the API port is held");
    assert!(format!("{err:#}").contains("binding"), "{err:#}");
    let alert = alerts.try_recv().expect("an alert");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("history_index_discarded", "WARN"));
    // What the node left is a fresh index, which takes the log from its start with no alert.
    let cfg = HistorySection { dir: index, retention: RETENTION, marks: None };
    let reopened = open_history(&cfg, &kept).unwrap();
    assert_eq!(reopened.resume_at(), Offset::start_of(0));
    assert!(alerts.try_recv().is_err(), "one alert");
}

/// An index the node must discard but cannot remove, beside a write-ahead log the host replaced
/// with a directory, stops the node with a Critical alert before recovery writes to the log.
#[tokio::test]
async fn a_history_index_the_node_cannot_discard_stops_it_before_it_writes_to_the_log() {
    let (kept, index) = index_ahead_of_log("node-history-stuck").await;
    fs::create_dir(index.join(format!("{INDEX_FILE}-wal"))).unwrap();
    // A listing a crash cut short, which recovery trims.
    let mut manifest = OpenOptions::new().append(true).open(kept.join(MANIFEST)).unwrap();
    manifest.write_all(br#"{"file":"#).unwrap();
    let before = files(&kept);

    let mut cfg = dev_config();
    cfg.alerts.outbox.dir = scratch_dir(&format!("node-outbox-{}", rand::random::<u64>()));
    (cfg.node.log_dir, cfg.history.dir) = (kept.clone(), index);
    cfg.node.listen = "127.0.0.1:0".parse().unwrap();
    cfg.metrics.listen = "127.0.0.1:0".parse().unwrap();
    let (_guard, mut alerts) = capture_alerts();
    let (telemetry, _layer) = telemetry();
    let refused = timeout(Duration::from_secs(10), run(cfg, telemetry)).await.expect("the node stops in time");
    let err = refused.expect_err("the index cannot be discarded");
    assert!(format!("{err:#}").contains("removing"), "{err:#}");
    let levels: Vec<_> = iter::from_fn(|| alerts.try_recv().ok())
        .map(|alert| (alert["alert"].clone(), alert["level"].clone()))
        .collect();
    let expected = [("history_index_discarded", "WARN"), ("history_recovery_failed", "ERROR")];
    assert_eq!(levels, expected.map(|(alert, level)| (alert.to_owned(), level.to_owned())));
    assert_eq!(files(&kept), before, "the log is as it was found");
}

#[tokio::test]
async fn a_genesis_cut_short_stops_the_node_before_it_writes_to_the_log() {
    let (dir, index) = (scratch_dir("node-genesis-cut"), scratch_dir("node-genesis-cut-index"));
    TestSequencer::start_legacy(&dir, 1024).stop().await;
    // As if the boot that ran genesis died before its house capital credit reached disk.
    let segment = files(&dir).into_keys().filter(|name| segment_seq(name).is_some()).max().expect("a segment");
    let text = fs::read_to_string(dir.join(&segment)).unwrap();
    let credit = text.find(r#""cmd":{"type":"house_capital""#).expect("the credit is in the last segment");
    let line = text[..credit].rfind('\n').expect("a command before it");
    fs::write(dir.join(&segment), &text[..line + 1]).unwrap();
    let before = files(&dir);

    let mut cfg = dev_config();
    cfg.alerts.outbox.dir = scratch_dir(&format!("node-outbox-{}", rand::random::<u64>()));
    (cfg.node.log_dir, cfg.history.dir) = (dir.clone(), index);
    cfg.node.listen = "127.0.0.1:0".parse().unwrap();
    cfg.metrics.listen = "127.0.0.1:0".parse().unwrap();
    let (telemetry, _layer) = telemetry();
    let refused = timeout(Duration::from_secs(10), run(cfg, telemetry)).await.expect("the node stops in time");
    let err = refused.expect_err("genesis did not finish");
    assert!(format!("{err:#}").contains("did not reach the checkpoint after its house capital credit"), "{err:#}");
    assert_eq!(files(&dir), before, "the log is as it was found");
}

/// A genesis the engine would reject, in a config that skipped validation, stops the node with a
/// Critical alert before anything reaches the log.
#[tokio::test]
async fn a_genesis_the_engine_would_reject_stops_the_node_with_an_alert_and_writes_nothing() {
    let (dir, index) = (scratch_dir("node-genesis-refused"), scratch_dir("node-genesis-refused-index"));
    let mut cfg = dev_config();
    cfg.alerts.outbox.dir = scratch_dir(&format!("node-outbox-{}", rand::random::<u64>()));
    (cfg.node.log_dir, cfg.history.dir) = (dir.clone(), index);
    cfg.node.listen = "127.0.0.1:0".parse().unwrap();
    cfg.genesis.benchmarks[0].annual_bps = 40_000;
    cfg.metrics.listen = "127.0.0.1:0".parse().unwrap();
    let (_guard, mut alerts) = capture_alerts();
    let (telemetry, _layer) = telemetry();
    let refused = timeout(Duration::from_secs(10), run(cfg, telemetry)).await.expect("the node stops in time");
    let err = refused.expect_err("the engine rejects the benchmark");
    assert!(format!("{err:#}").contains("genesis benchmark SOFR: the engine rejects it"), "{err:#}");
    let raised: Vec<_> = iter::from_fn(|| alerts.try_recv().ok())
        .map(|alert| (alert["alert"].clone(), alert["level"].clone()))
        .collect();
    assert_eq!(raised, [("genesis_refused".to_owned(), "ERROR".to_owned())]);
    assert!(log_entries(&dir).is_empty(), "nothing reached the log");
}

// ── Helpers ──

/// Telemetry with no webhooks, and the layer that feeds it, kept so the router stays open; the
/// test's own subscriber sees the alert lines.
fn telemetry() -> (Telemetry, AlertLayer) {
    let metrics = Arc::new(Metrics::new());
    let (layer, alerts) = alerts::channel(Routes::default(), metrics.clone());
    (Telemetry { metrics, alerts, pages_critical: false }, layer)
}

/// A log in a new directory named after `name`, as the disk keeps it when the power fails after
/// its first deposit, and an index that took a second deposit the log then lost.
async fn index_ahead_of_log(name: &str) -> (PathBuf, PathBuf) {
    let (dir, index) = (scratch_dir(name), scratch_dir(&format!("{name}-index")));
    let node = TestSequencer::start_legacy(&dir, 1024);
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    node.submit(deposit(1, "dep-1")).await;
    let kept = scratch_dir(&format!("{name}-kept"));
    fs::create_dir_all(&kept).unwrap();
    for file in fs::read_dir(&dir).unwrap() {
        let file = file.unwrap();
        fs::copy(file.path(), kept.join(file.file_name())).unwrap();
    }
    node.submit(deposit(1, "dep-2")).await;
    node.stop().await;
    history.finish().await.unwrap();
    (kept, index)
}

/// Legacy history takes no captures and resumes from its own cursor, so shutdown stops it at once
/// instead of waiting out the drain grace.
#[tokio::test]
async fn shutdown_stops_legacy_history_without_waiting_for_it() {
    let mut history = tokio::task::JoinSet::new();
    let handle = history.spawn(std::future::pending::<anyhow::Result<()>>());
    tokio::time::timeout(std::time::Duration::from_millis(500), finish_history(history, None))
        .await
        .expect("no drain grace for legacy history");
    tokio::task::yield_now().await;
    assert!(handle.is_finished(), "legacy history is aborted");
}

/// Every file in `dir` and its bytes.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let names = fs::read_dir(dir).unwrap().map(|file| file.unwrap().file_name().into_string().unwrap());
    names.map(|name| (name.clone(), fs::read(dir.join(name)).unwrap())).collect()
}

mod custody_preflight;

/// Recovers before any append, checking history against that read during full replay or against
/// its cursor's segment after authenticated snapshot recovery. A mismatched index is rebuilt.
/// Error cases:
/// - the history index cannot be opened, nor a fresh one in its place (see `HistoryCheck`)
/// - the log cannot be read or replayed (see `Sequencer::read_log`)
pub(crate) fn read_log_and_history(cfg: &NodeConfig) -> anyhow::Result<(ReplayedLog, crate::history::History)> {
    let key = crate::tee::EnclaveKey::test_key();
    let (log, plan) = read_log_and_history_observe(cfg, Some(AccountId(key.address())), None, |_| {})?;
    plan.finish_with_log(log, Arc::new(key))
}

/// Replays and checks history while exposing the same entries for the on-chain anchor check.
/// Error cases:
/// - the log or history index cannot be read or verified
pub(crate) fn read_log_and_history_observe(
    cfg: &NodeConfig,
    signer: Option<AccountId>,
    parent: Option<crate::log::CheckpointParent>,
    mut observe: impl FnMut(&crate::log::LogEntry),
) -> anyhow::Result<(ReplayedLog, HistoryPlan)> {
    // Tests that do not assert on marks seed a scratch cache they discard with the log.
    // Outside `cfg.history.dir`: refused-startup byte-preservation snapshots cover that dir.
    let scratch = marks_store(cfg)?;
    super::read_log_and_history_authorities(
        cfg,
        signer,
        parent,
        Default::default(),
        &scratch,
        &crate::test_support::stats(),
        &mut observe,
    )
}

/// A scratch marks store outside `cfg.history.dir`, which refused-startup byte-preservation
/// snapshots cover. One directory per call: tests of this binary run in parallel threads.
fn marks_store(cfg: &NodeConfig) -> anyhow::Result<std::rc::Rc<std::cell::RefCell<crate::history::marks::Store>>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let _ = cfg;
    let dir = crate::test_support::scratch_dir(&format!("marks-observe-{}", N.fetch_add(1, Ordering::Relaxed)));
    Ok(std::rc::Rc::new(std::cell::RefCell::new(crate::history::marks::Store::open_in(
        &dir,
        crate::history::marks::Retentions::defaults(),
        crate::tee::EnclaveKey::test_key().marks_key(),
    )?)))
}
