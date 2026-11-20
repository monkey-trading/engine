//! The capture cadence through the node's startup order: a cache at a start's first checkpoint, then
//! one every `[snapshots] interval` of sequenced time, and one at shutdown; a capture that finds one in
//! flight, or that the history owner cannot take, stays due; a handover's sealed log is not captured;
//! a node without a stats worker encodes its caches itself; a restart resumes from the newest.

use std::sync::Arc;
use std::time::Duration;

use crate::clock::test_clock::TestClock;
use crate::clock::{Clock, SystemClock};
use crate::log::LogEntry;
use crate::node::paired_tests::anchor::seq_of;
use crate::node::paired_tests::harness::{Node, configuration};
use crate::node::paired_tests::publication::{collect_alerts, write_alert_lines};
use crate::sequencer::Request;
use crate::snapshot;
use crate::test_support::{deposit, log_entries};

const INTERVAL: Duration = Duration::from_secs(3_600);

#[tokio::test]
async fn captures_wait_for_the_interval_and_shutdown_captures_the_last_checkpoint() {
    let mut cfg = configuration("paired-cadence");
    cfg.snapshots.as_mut().unwrap().interval = INTERVAL;
    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let node = Node::start_at(cfg, clock.clone()).await.unwrap();
    // Checkpoints every three commands: several fall within the interval, and only the first is due.
    deposits(&node, "first", 9).await;
    let first = caches(&node);
    assert_eq!(first.len(), 1, "one cache within the interval: {first:?}");
    // The interval passes in sequenced time: the next checkpoint's capture is due, and only it.
    clock.advance(INTERVAL);
    deposits(&node, "second", 9).await;
    let second = caches(&node);
    assert_eq!(second.len(), 2, "one more cache once the interval passed: {second:?}");
    assert!(seq_of(&second[0]) > seq_of(&first[0]));
    let (cfg, reader) = node.stop().await;
    drop(reader);
    // Shutdown captures the last checkpoint, which covers the last command, whatever the interval.
    let last = log_entries(&cfg.node.log_dir)
        .iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Command(command) => Some(command.sequenced.seq),
            _ => None,
        })
        .unwrap();
    let stopped = snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap();
    assert_eq!(stopped.len(), 3, "the shutdown capture: {stopped:?}");
    assert_eq!(seq_of(&stopped[0]), last, "the shutdown cache covers the last command");
    let node = Node::start_at(cfg, clock).await.unwrap();
    assert_eq!(node.snapshot_seq, Some(last), "the restart resumes from the shutdown cache");
    node.stop_after_signing("paired-cadence-restart").await;
}

#[tokio::test]
async fn a_capture_that_finds_one_in_flight_stays_due() {
    let mut cfg = configuration("paired-cadence-in-flight");
    cfg.snapshots.as_mut().unwrap().interval = INTERVAL;
    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let node = Node::start_at(cfg, clock.clone()).await.unwrap();
    deposits(&node, "first", 3).await;
    assert_eq!(caches(&node).len(), 1, "the start's first checkpoint is captured");
    // The interval passes while a capture is in flight: the due checkpoints are not captured.
    clock.advance(INTERVAL);
    let in_flight = node.sealing.try_permit().expect("no capture is in flight");
    deposits(&node, "in-flight", 3).await;
    assert_eq!(caches(&node).len(), 1, "no capture while another is in flight");
    // The capture stays due, so the first checkpoint after it lands is captured, inside the interval
    // a capture at a skipped checkpoint would have started.
    drop(in_flight);
    deposits(&node, "landed", 3).await;
    assert_eq!(caches(&node).len(), 2, "the capture stays due past the one in flight");
    node.stop().await;
}

#[tokio::test]
async fn a_capture_the_history_owner_cannot_take_stays_due() {
    let mut cfg = configuration("paired-cadence-refused");
    cfg.snapshots.as_mut().unwrap().interval = INTERVAL;
    let (collector, router) = collect_alerts();
    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let node = Node::start_at(cfg, clock.clone()).await.unwrap();
    deposits(&node, "first", 3).await;
    // A closed history queue refuses the hand-over, as a full one does. Each checkpoint past the
    // interval tries again: a refused capture stays due.
    node.stop_history().await;
    clock.advance(INTERVAL);
    for n in 0..9 {
        node.submit(deposit(1, &format!("refused-{n}"))).await;
    }
    drop(collector);
    let failed = write_alert_lines(router).into_iter().filter(|line| line.severity.is_some()).count();
    assert!(failed >= 3, "every checkpoint past the interval tries the capture again: {failed} refusals");
}

#[tokio::test]
async fn a_handover_takes_no_capture_of_the_log_it_sealed() {
    let mut cfg = configuration("paired-cadence-handover");
    cfg.snapshots.as_mut().unwrap().interval = INTERVAL;
    let (collector, router) = collect_alerts();
    let node = Node::start(cfg).await.unwrap();
    deposits(&node, "first", 4).await;
    let before = caches(&node);
    // The handover's final checkpoint falls inside the interval, so it is not captured when written,
    // and the stop that follows takes no capture of the sealed log either.
    node.refresh_time().await;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    node.tx.send(Request::FinalCheckpoint(stop)).await.unwrap();
    stopped.await.unwrap().expect("the handover's final checkpoint seals the log");
    let (cfg, reader) = node.stop().await;
    drop((reader, collector));
    let lines = write_alert_lines(router);
    assert!(lines.iter().all(|line| line.severity.is_none()), "no capture of the sealed log fails: {lines:#?}");
    assert_eq!(snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap(), before);
}

#[tokio::test]
async fn a_node_without_a_stats_worker_encodes_its_caches_itself() {
    let node = Node::start_without_stats_worker(configuration("paired-no-stats-worker")).await.unwrap();
    deposits(&node, "inline", 6).await;
    assert!(!caches(&node).is_empty(), "the sequencer encodes the cache");
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let node = Node::start(cfg).await.unwrap();
    assert!(node.snapshot_seq.is_some(), "a cache the sequencer encoded resumes the node");
    node.stop_after_signing("paired-no-stats-worker-restart").await;
}

// ── Helpers ──

/// Sequences `count` deposits labelled `label`, each indexed before the next, so no capture finds the
/// previous one in flight.
async fn deposits(node: &Node, label: &str, count: usize) {
    for n in 0..count {
        node.submit(deposit(1, &format!("{label}-{n}"))).await;
        node.history_caught_up().await;
    }
}

/// The node's caches, newest first.
fn caches(node: &Node) -> Vec<std::path::PathBuf> {
    snapshot::candidates_readonly(node.cfg.snapshots.as_ref().unwrap()).unwrap()
}
