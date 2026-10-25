//! Real producer/actor failures must keep publication attempts and protected versions bounded, and
//! page as one alert episode.

use std::collections::BTreeSet;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use engine_core::command::Command;
use tokio::time::timeout;
use tracing::subscriber::{DefaultGuard, set_default};
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry;

use crate::alerts::{self, AlertLine, Router, Routes, Severity, is_alert_callsite};
use crate::metrics::Metrics;
use crate::node::paired_tests::harness::{Node, configuration};
use crate::snapshot::{self, head};
use crate::test_support::{deposit, keep_callsites_interested};

#[tokio::test]
async fn repeated_history_publication_failures_do_not_accumulate_unpublished_catalog_pins() {
    let node = Node::start(configuration("paired-failed-publication-pins")).await.unwrap();
    for n in 0..6 {
        node.submit(deposit(1, &format!("durable-{n}"))).await;
    }
    let before = at_frontier(&node).await;
    assert!(before.history_pins > 0, "actual durable history roots are protected");
    let caches = node.cfg.snapshots.as_ref().unwrap();
    // This blocks only H staging cleanup; signed T publication and authoritative logging continue.
    // Inventory correctly ignores a staging name, so the failure is in the real publication path.
    let blocked_stage = caches.dir.join("history-head-99999999999999999999.json.tmp");
    fs::create_dir(&blocked_stage).unwrap();
    for n in 0..30 {
        node.submit(deposit(1, &format!("failed-attempt-{n}"))).await;
    }
    let failed = at_frontier(&node).await;
    assert!(failed.last_entry > before.last_entry, "history still consumes the authoritative command/checkpoint feed");
    assert!(failed.catalog_pins <= before.catalog_pins, "unpublished failed C attempts cannot accumulate root leases");
    assert!(failed.history_pins <= before.history_pins + 1, "only one uncertain optional publication may add a pin");
    fs::remove_dir(blocked_stage).unwrap();
    for _ in 0..6 {
        node.submit(Command::Tick).await;
    }
    let repaired = at_frontier(&node).await;
    assert!(repaired.catalog_pins <= caches.retain.get());
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let restarted = Node::start(cfg).await.unwrap();
    assert!(restarted.snapshot_seq.is_some(), "real publication resumes after the fault");
    restarted.stop().await;
}

#[tokio::test]
async fn failed_reference_reconciliation_keeps_old_pins_before_any_gc_or_pointer_allocation() {
    let node = Node::start(configuration("paired-pin-release-barrier")).await.unwrap();
    // History catches up after each command, so no capture finds the previous one in flight.
    for n in 0..9 {
        node.submit(deposit(1, &format!("barrier-{n}"))).await;
        at_frontier(&node).await;
    }
    let before = at_frontier(&node).await;
    assert!(before.history_pins >= 2, "multiple actual retained roots exist");
    let caches = node.cfg.snapshots.as_ref().unwrap();
    let heads = head::candidates_readonly(caches).unwrap();
    assert_eq!(heads.len(), 2);
    // Preserve every byte while modeling metadata whose removal has not passed reconciliation.
    let saved: Vec<_> = heads.iter().map(|path| (path.clone(), path.with_extension("json.preserved"))).collect();
    for (path, saved) in &saved {
        fs::rename(path, saved).unwrap();
    }
    let journal = caches.dir.join("authenticated-generations.v1");
    let journal_saved = caches.dir.join("authenticated-generations.v1.preserved");
    fs::rename(&journal, &journal_saved).unwrap();
    fs::create_dir(&journal).unwrap();
    for n in 0..12 {
        node.submit(deposit(1, &format!("barrier-held-{n}"))).await;
    }
    let failed = at_frontier(&node).await;
    assert_eq!(failed.history_pins, before.history_pins, "failed barrier cannot release or allocate root pins");
    assert_eq!(failed.catalog_pins, before.catalog_pins, "uncertain retained data remains protected");
    fs::remove_dir(&journal).unwrap();
    fs::rename(&journal_saved, &journal).unwrap();
    for (path, saved) in saved {
        fs::rename(saved, path).unwrap();
    }
    for _ in 0..6 {
        node.submit(Command::Tick).await;
    }
    assert!(at_frontier(&node).await.last_entry > failed.last_entry);
    node.stop().await;
}

/// Failed captures at successive checkpoints are one `snapshot_write_failed` episode, each naming its
/// checkpoint's seq in the page context. The first capture stored after a start or after failures
/// resolves it once, so an episode that a stopped run left open closes after the restart.
#[tokio::test]
async fn failed_captures_are_one_alert_episode_until_a_capture_is_stored_even_across_restart() {
    let cfg = configuration("paired-write-alert-episode");
    // A directory under a cache staging name fails the cleanup that precedes every cache write.
    let blocked_stage = cfg.snapshots.as_ref().unwrap().dir.join("99999999999999999999-0000000000.json.tmp");
    let (collector, router) = collect_alerts();
    let node = Node::start(cfg).await.unwrap();
    submit_deposits(&node, "stored", 3).await;
    let caches = node.cfg.snapshots.as_ref().unwrap();
    assert!(!snapshot::candidates_readonly(caches).unwrap().is_empty(), "a capture was stored before the fault");
    fs::create_dir_all(&blocked_stage).unwrap();
    submit_deposits(&node, "unstored", 9).await;
    let (cfg, reader) = node.stop().await;
    drop((reader, collector));
    let stopped = write_alert_lines(router);
    let [started, failed @ ..] = stopped.as_slice() else { panic!("no snapshot_write_failed line") };
    assert_eq!(started.severity, None, "the first stored capture after a start resolves: {stopped:#?}");
    assert_one_episode(failed, &started.key(), &stopped);

    fs::remove_dir(&blocked_stage).unwrap();
    let (collector, router) = collect_alerts();
    let node = Node::start(cfg).await.unwrap();
    submit_deposits(&node, "restarted", 6).await;
    fs::create_dir_all(&blocked_stage).unwrap();
    submit_deposits(&node, "unstored-again", 9).await;
    fs::remove_dir(&blocked_stage).unwrap();
    submit_deposits(&node, "recovered", 6).await;
    node.stop().await;
    drop(collector);
    let restarted = write_alert_lines(router);
    let [reopened, failed @ .., recovered] = restarted.as_slice() else { panic!("{restarted:#?}") };
    assert_eq!((reopened.severity, reopened.key()), (None, started.key()), "the restart resolves: {restarted:#?}");
    assert_one_episode(failed, &started.key(), &restarted);
    assert_eq!((recovered.severity, recovered.key()), (None, started.key()), "recovery resolves: {restarted:#?}");
}

/// A capture the sequencer cannot hand to the history owner raises `snapshot_write_failed` on the
/// shared key and leaves the alert open, so the next capture the history owner stores resolves it.
#[tokio::test]
async fn a_capture_the_sequencer_cannot_hand_over_leaves_the_alert_open() {
    let (collector, router) = collect_alerts();
    let node = Node::start(configuration("paired-sequencer-write-alert")).await.unwrap();
    submit_deposits(&node, "stored", 3).await;
    let caches = node.cfg.snapshots.as_ref().unwrap();
    assert!(!snapshot::candidates_readonly(caches).unwrap().is_empty(), "a capture was stored before the failure");
    assert!(!node.sealing.write_alert.take(), "the stored capture cleared the shared mark");
    // A closed history queue refuses the sequencer's hand-over, as a full one does.
    node.stop_history().await;
    for n in 0..3 {
        node.submit(deposit(1, &format!("unhanded-{n}"))).await;
    }
    drop(collector);
    let lines = write_alert_lines(router);
    let [started, failed @ ..] = lines.as_slice() else { panic!("no snapshot_write_failed line") };
    assert_eq!(started.severity, None, "the first stored capture after a start resolves: {lines:#?}");
    assert!(!failed.is_empty(), "the sequencer raised the alert: {lines:#?}");
    for line in failed {
        assert_eq!((line.severity, line.key()), (Some(Severity::Warning), started.key()), "{lines:#?}");
        assert!(line.summary.starts_with("checkpoint remains in log"), "{lines:#?}");
    }
    assert!(node.sealing.write_alert.take(), "the next stored capture resolves the sequencer's failure");
}

/// A shutdown that times out waiting for an in-flight capture raises `snapshot_write_failed` on the
/// shared key and leaves the alert open, so the next capture the history owner stores resolves it.
#[tokio::test]
async fn a_shutdown_that_cannot_wait_for_a_capture_leaves_the_alert_open() {
    let (collector, router) = collect_alerts();
    let node = Node::start(configuration("paired-shutdown-write-alert")).await.unwrap();
    submit_deposits(&node, "stored", 3).await;
    assert!(!node.sealing.write_alert.take(), "a stored capture cleared the shared mark");
    let sealing = node.sealing.clone();
    // Holding the only capture permit stands in for a capture still in flight at shutdown.
    let in_flight = sealing.try_permit().expect("no capture is in flight");
    submit_deposits(&node, "uncaptured", 3).await;
    node.stop().await;
    drop((in_flight, collector));
    let lines = write_alert_lines(router);
    let [started, failed] = lines.as_slice() else {
        panic!("a start resolve, then the shutdown's failure: {lines:#?}")
    };
    assert_eq!(started.severity, None, "{lines:#?}");
    assert_eq!((failed.severity, failed.key()), (Some(Severity::Warning), started.key()), "{lines:#?}");
    assert!(failed.summary.starts_with("the shutdown checkpoint is not cached"), "{lines:#?}");
    assert!(sealing.write_alert.take(), "the next stored capture resolves the shutdown's failure");
}

/// Observes the actual blocking owner after every physical entry in the producer's fixed frontier.
async fn at_frontier(node: &Node) -> crate::history::runtime::TestState {
    let target = node.manifest.borrow().flushed;
    timeout(Duration::from_secs(30), async {
        loop {
            let state = node.sealing.inspect().await.unwrap();
            if state.last_entry >= target {
                return state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("supervised history owner reaches the actual published frontier")
}

/// Collects alert lines through the node's alert layer, the history owner's included, until the
/// guard drops.
fn collect_alerts() -> (DefaultGuard, Router) {
    keep_callsites_interested();
    let (layer, router) = alerts::channel(Routes::default(), Arc::new(Metrics::new()));
    (set_default(registry().with(layer.with_filter(filter_fn(is_alert_callsite)))), router)
}

/// The `snapshot_write_failed` lines `router` received, in order.
fn write_alert_lines(router: Router) -> Vec<AlertLine> {
    router.queued().into_iter().filter(|line| line.name == "snapshot_write_failed").collect()
}

/// Asserts that `failed` are Warnings at two or more distinct checkpoints, all keyed `key`.
fn assert_one_episode(failed: &[AlertLine], key: &str, lines: &[AlertLine]) {
    assert!(failed.len() >= 2, "captures failed at several checkpoints: {lines:#?}");
    assert!(failed.iter().all(|line| line.severity == Some(Severity::Warning)), "{lines:#?}");
    assert!(failed.iter().all(|line| line.key() == key), "one episode key: {lines:#?}");
    let contexts: BTreeSet<_> = failed.iter().map(|line| line.context.as_str()).collect();
    assert_eq!(contexts.len(), failed.len(), "each failure names its checkpoint: {lines:#?}");
    assert!(contexts.iter().all(|context| context.starts_with("capture{seq=")), "{lines:#?}");
}

/// Submits `count` deposits, waiting for history after each, so no capture finds the previous one
/// still in flight.
async fn submit_deposits(node: &Node, label: &str, count: usize) {
    for n in 0..count {
        node.submit(deposit(1, &format!("{label}-{n}"))).await;
        at_frontier(node).await;
    }
}
