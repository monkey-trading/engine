//! The capture cadence through the node's startup order: a cache at a start's first checkpoint, then
//! one every `[snapshots] interval` of sequenced time, and one at shutdown; a capture that finds one in
//! flight, or that a stopped history owner cannot take, stays due; an owner whose queue is held takes a
//! capture and completes it once it reaches the checkpoint, with a warning once it has waited a minute, and
//! leaves its encoding wait at once when stopped; a handover's sealed log, or a checkpoint
//! a withdrawal entry follows, is not captured at shutdown;
//! a node without a stats worker encodes its caches itself; a restart resumes from the newest.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::alerts::Router;
use crate::clock::test_clock::TestClock;
use crate::clock::{Clock, SystemClock};
use crate::log::LogEntry;
use crate::node::paired_tests::anchor::seq_of;
use crate::node::paired_tests::harness::{HANG, Node, configuration};
use crate::node::paired_tests::publication::{collect_alerts, write_alert_lines};
use crate::sequencer::Request;
use crate::snapshot;
use crate::snapshot::sealing::{CaptureStage, ENCODE_WAIT};
use crate::test_support::{deposit, log_entries, withdraw};

pub(super) const INTERVAL: Duration = Duration::from_secs(3_600);

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
    // A stopped history owner refuses the hand-over. Each checkpoint past the interval tries again: a
    // refused capture stays due.
    node.stop_history().await;
    clock.advance(INTERVAL);
    for n in 0..9 {
        node.submit(deposit(1, &format!("refused-{n}"))).await;
    }
    drop(collector);
    let failed = write_alert_lines(router).into_iter().filter(|line| line.severity.is_some()).count();
    assert!(failed >= 3, "every checkpoint past the interval tries the capture again: {failed} refusals");
}

/// A capture due while the history owner's queue has no free slot is still taken, with no alert, and the
/// owner, its input no longer ahead of it, completes it once it reaches the checkpoint.
#[tokio::test]
async fn an_owner_whose_queue_is_held_takes_a_capture_and_completes_it_at_its_checkpoint() {
    let mut cfg = configuration("paired-cadence-behind");
    cfg.snapshots.as_mut().unwrap().interval = INTERVAL;
    let (collector, router) = collect_alerts();
    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let node = Node::start_at(cfg, clock.clone()).await.unwrap();
    deposits(&node, "first", 3).await;
    let before = caches(&node);
    clock.advance(INTERVAL);
    // Commands up to a checkpoint, its capture due, while the history owner's queue has no free slot.
    let held = node.sealing.hold_queue();
    assert!(!held.is_empty(), "the queue had free slots to take");
    let last = to_checkpoint(&node, "behind").await;
    assert_eq!(caches(&node), before, "the owner reached the checkpoint with its queue full");
    drop(held);
    node.history_caught_up().await;
    let taken = caches(&node);
    assert_eq!(taken.len(), before.len() + 1, "the capture behind the full queue: {taken:?}");
    assert_eq!(seq_of(&taken[0]), last, "the cache is the checkpoint whose capture was due");
    let (_, reader) = node.stop().await;
    drop((reader, collector));
    let lines = write_alert_lines(router);
    assert!(lines.iter().all(|line| line.severity.is_none()), "no capture was refused: {lines:#?}");
}

/// A capture due while the one handed over before it has waited, for an owner that has not taken it, past the
/// interval and a minute at least alerts, and the owner's storing of that capture, once its index reaches
/// it, resolves it. With a one-second interval the minute decides, and with an hour the next capture is due
/// only after the minute.
#[tokio::test]
async fn a_capture_the_owner_has_not_stored_for_a_minute_alerts_until_it_is_stored() {
    let (second, minute) = (Duration::from_secs(1), Duration::from_secs(60));
    for (interval, late) in [(second, minute), (INTERVAL, INTERVAL)] {
        let mut cfg = configuration(&format!("paired-cadence-late-{}", interval.as_secs()));
        cfg.snapshots.as_mut().unwrap().interval = interval;
        let (collector, mut router) = collect_alerts();
        let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
        let node = Node::start_at(cfg, clock.clone()).await.unwrap();
        deposits(&node, "first", 3).await;
        let before = caches(&node);
        clock.advance(interval);
        let held = node.sealing.hold_queue();
        let handed = to_checkpoint(&node, "handed").await;
        let firing = |router: &mut Router| {
            let lines = router.take();
            lines.into_iter().filter(|line| line.name == "snapshot_write_failed" && line.severity.is_some()).count()
        };
        router.take();
        clock.advance(late - second);
        to_checkpoint(&node, "early").await;
        assert_eq!(firing(&mut router), 0, "a capture waiting {:?} alerted at interval {interval:?}", late - second);
        clock.advance(second);
        to_checkpoint(&node, "late").await;
        assert!(firing(&mut router) > 0, "a capture waiting {late:?} raised nothing at interval {interval:?}");
        drop(held);
        node.history_caught_up().await;
        let stored = caches(&node);
        assert_eq!(stored.len(), before.len() + 1, "the late capture is stored: {stored:?}");
        assert_eq!(seq_of(&stored[0]), handed, "the cache is the checkpoint first handed over");
        let lines: Vec<_> = router.take().into_iter().filter(|line| line.name == "snapshot_write_failed").collect();
        assert!(
            matches!(lines.as_slice(), [resolve] if resolve.severity.is_none()),
            "storing the late capture, and only that, resolves the alert: {lines:#?}"
        );
        drop(node.stop().await);
        drop((collector, router));
    }
}

/// A history owner stopped while its capture waits for an engine encoding that never comes leaves at the
/// end of its next poll, not at `ENCODE_WAIT`, through the wait's stop: the capture fails and no cache is
/// stored for it.
#[tokio::test]
async fn an_owner_stopped_while_its_capture_waits_for_its_encoding_leaves_at_once() {
    let mut cfg = configuration("paired-cadence-stopped-encoding");
    cfg.snapshots.as_mut().unwrap().interval = INTERVAL;
    let (collector, router) = collect_alerts();
    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let node = Node::start_at(cfg, clock.clone()).await.unwrap();
    deposits(&node, "first", 3).await;
    let before = caches(&node);
    clock.advance(INTERVAL);
    let polls = node.sealing.encode_polls.load(Ordering::Acquire);
    node.stats.hold_encodes();
    to_checkpoint(&node, "held").await;
    // A poll ended without the encoding held back: the owner is in the wait when it is stopped.
    let since = Instant::now();
    while node.sealing.encode_polls.load(Ordering::Acquire) == polls {
        assert!(since.elapsed() < HANG, "the owner never waited for the held encoding");
        tokio::task::yield_now().await;
    }
    let snapshots = node.cfg.snapshots.clone().unwrap();
    let took = node.request_history_stop().await;
    assert!(took < ENCODE_WAIT / 10, "the stopped owner took {took:?} to leave its encoding wait");
    assert_eq!(snapshot::candidates_readonly(&snapshots).unwrap(), before, "a cache was stored for the held capture");
    drop(collector);
    let lines = write_alert_lines(router);
    assert!(
        lines
            .iter()
            .any(|line| line.severity.is_some() && line.summary.contains("stopped before the engine was encoded")),
        "the capture did not fail through the wait's stop: {lines:#?}"
    );
}

/// A capture of the last command's checkpoint fails after its slot is reserved, in the seal, the cache's
/// fields or the engine's encoding, and shutdown still caches that checkpoint.
#[tokio::test]
async fn shutdown_caches_a_checkpoint_whose_capture_failed_after_its_slot_was_reserved() {
    for stage in [CaptureStage::Seal, CaptureStage::Fields, CaptureStage::Encode] {
        let mut cfg = configuration(&format!("paired-cadence-failed-{stage:?}"));
        cfg.snapshots.as_mut().unwrap().interval = INTERVAL;
        let (collector, router) = collect_alerts();
        let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
        let node = Node::start_at(cfg, clock.clone()).await.unwrap();
        deposits(&node, "first", 3).await;
        let before = caches(&node);
        clock.advance(INTERVAL);
        node.sealing.fail_next(stage);
        // Commands up to a checkpoint, whose due capture fails once.
        let last = to_checkpoint(&node, "failed").await;
        node.history_caught_up().await;
        assert_eq!(caches(&node), before, "{stage:?}: the capture failed");
        let (cfg, reader) = node.stop().await;
        drop((reader, collector));
        let failed = write_alert_lines(router).into_iter().filter(|line| line.severity.is_some()).count();
        assert!(failed >= 1, "{stage:?}: the failed capture alerts: {failed}");
        let stopped = snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap();
        assert_eq!(stopped.len(), before.len() + 1, "{stage:?}: the shutdown capture: {stopped:?}");
        assert_eq!(seq_of(&stopped[0]), last, "{stage:?}: the shutdown cache covers the failed checkpoint");
    }
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

/// Mock custody signs an approval right after the checkpoint that covers it, which the interval leaves
/// uncaptured; shutdown then takes no capture of it, since a cache seals the log at its checkpoint.
#[tokio::test]
async fn a_withdrawal_signed_after_an_uncaptured_checkpoint_leaves_it_uncaptured() {
    let mut cfg = configuration("paired-cadence-withdrawal");
    cfg.snapshots.as_mut().unwrap().interval = INTERVAL;
    let (collector, router) = collect_alerts();
    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let node = Node::start_at(cfg, clock).await.unwrap();
    deposits(&node, "first", 3).await;
    let before = caches(&node);
    node.submit(withdraw(1)).await;
    node.history_caught_up().await;
    let (cfg, reader) = node.stop().await;
    drop((reader, collector));
    let lines = write_alert_lines(router);
    assert!(lines.iter().all(|line| line.severity.is_none()), "no shutdown capture fails: {lines:#?}");
    assert_eq!(snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap(), before);
}

/// A node whose stats worker stopped encodes its caches itself, with no replay state, as no worker folds it: they
/// resume a node, but one publishing daily snapshots, which need that state, refuses them, each with Warning
/// `snapshot_rejected`, and replays the log instead.
#[tokio::test]
async fn a_node_without_a_stats_worker_encodes_its_caches_itself() {
    let node = Node::start_without_stats_worker(configuration("paired-no-stats-worker")).await.unwrap();
    deposits(&node, "inline", 6).await;
    assert!(!caches(&node).is_empty(), "the sequencer encodes the cache");
    let (mut cfg, reader) = node.stop().await;
    drop(reader);
    let root = cfg.node.log_dir.parent().unwrap().to_owned();
    cfg.daily_snapshots = Some(crate::config::DailySnapshotsSection { dir: root.join("daily") });
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let (log, plan) = crate::node::tests::read_log_and_history_observe(&cfg, None, |_| {}).await.unwrap();
    let rejected = std::iter::from_fn(|| alerts.try_recv().ok()).filter(|alert| alert["alert"] == "snapshot_rejected");
    assert!(rejected.count() > 0, "the caches are rejected");
    assert_eq!(log.snapshot_sequence(), None, "the log replays");
    drop((log, plan));
    cfg.daily_snapshots = None;
    let node = Node::start(cfg).await.unwrap();
    assert!(node.snapshot_seq.is_some(), "a cache the sequencer encoded resumes the node");
    node.stop_after_signing("paired-no-stats-worker-restart").await;
}

// ── Helpers ──

/// Submits deposits until the next command ends with a checkpoint, and returns its seq.
pub(super) async fn to_checkpoint(node: &Node, label: &str) -> u64 {
    let every = node.cfg.node.checkpoint_every;
    let flushed = || node.manifest.borrow().flushed.expect("commands are logged").seq;
    let mut n = 0;
    while n == 0 || (flushed() + 1) % every != 0 {
        node.submit(deposit(1, &format!("{label}-{n}"))).await;
        n += 1;
    }
    flushed()
}

/// Sequences `count` deposits labelled `label`, each indexed before the next, so no capture finds the
/// previous one in flight.
pub(super) async fn deposits(node: &Node, label: &str, count: usize) {
    for n in 0..count {
        node.submit(deposit(1, &format!("{label}-{n}"))).await;
        node.history_caught_up().await;
    }
}

/// The node's caches, newest first.
pub(super) fn caches(node: &Node) -> Vec<std::path::PathBuf> {
    snapshot::candidates_readonly(node.cfg.snapshots.as_ref().unwrap()).unwrap()
}
