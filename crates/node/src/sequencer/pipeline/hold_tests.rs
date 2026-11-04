//! The held-sync bound: past `MAX_SYNC_HOLD` the sequencer takes no request, drained or new, and
//! pages Critical `log_sync_held` once — from any wait on the sync, with nothing arriving — and the
//! episode resolves once no batch in flight is past the bound, an earlier run's included.

use std::sync::mpsc::{Receiver as StdReceiver, Sender as StdSender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_core::command::Command;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::timeout;

use crate::log::Manifest;
use crate::sequencer::pipeline::hold::MAX_SYNC_HOLD;
use crate::sequencer::pipeline::tests::Verdict;
use crate::sequencer::pipeline::{Batch, MAX_IN_FLIGHT};
use crate::sequencer::{Request, Sequencer};
use crate::test_support::{capture_alerts_and_resolutions, dev_config, sample_around, scratch_dir, sequencer_config};

#[tokio::test(start_paused = true)]
async fn a_sync_held_past_the_bound_pages_once_and_stamps_nothing_until_it_finishes() {
    let (mut seq, permits, entered) = held_sequencer("pipeline-held");
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    stage_ticks(&mut seq, 1);
    entered.recv().expect("the thread starts the first sync");
    let (tx, mut rx) = mpsc::channel(4);
    // Inside the bound the next request is taken while the sync runs.
    tx.send(query()).await.unwrap();
    let taken = timeout(Duration::from_secs(1), seq.next_request(&mut rx)).await.expect("inside the bound");
    assert!(matches!(taken.unwrap(), Some(Request::Query(_))));
    assert!(alerts.try_recv().is_err(), "no page inside the bound");
    // Past it, the request waits behind the held sync, and the hold pages once however often the
    // sequencer looks.
    tokio::time::advance(MAX_SYNC_HOLD).await;
    tx.send(query()).await.unwrap();
    for _ in 0..2 {
        let held = timeout(MAX_SYNC_HOLD * 3, seq.next_request(&mut rx)).await;
        assert!(held.is_err(), "past the bound the sequencer takes no request");
    }
    let page = alerts.try_recv().expect("the hold pages");
    assert_eq!(page.get("alert").map(String::as_str), Some("log_sync_held"));
    assert_eq!(page.get("level").map(String::as_str), Some("ERROR"), "a held sync is Critical");
    assert!(alerts.try_recv().is_err(), "one page per hold");
    // A batch staged behind the held one, while the thread is inside the first sync, is a job of its
    // own and inside its own bound: once the held sync finishes, the episode ends while that batch's
    // sync is still waiting for its verdict. The sync finishes on the real disk, so real time resumes.
    tokio::time::resume();
    stage_ticks(&mut seq, 1);
    permits.send(Verdict::Pass).unwrap();
    entered.recv().expect("the thread starts the successor's sync on its own");
    let taken = timeout(Duration::from_secs(10), seq.next_request(&mut rx)).await.expect("a finished sync frees it");
    assert!(matches!(taken.unwrap(), Some(Request::Query(_))));
    let end = resolved.try_recv().expect("the finished sync resolves the hold");
    assert_eq!(end.get("alert_resolved").map(String::as_str), Some("log_sync_held"));
    assert_eq!(seq.pipeline.as_ref().unwrap().in_flight.len(), 1, "the successor's sync is still pending");
    assert!(alerts.try_recv().is_err(), "the release does not page again");
}

#[tokio::test(start_paused = true)]
async fn a_quiet_node_pages_a_hold_at_the_deadline() {
    // Nothing arrives: the wait itself wakes at the bound, with one batch pending and with the
    // pipeline full.
    for (label, batches) in [("pipeline-quiet-one", 1), ("pipeline-quiet-full", MAX_IN_FLIGHT)] {
        let (mut seq, _permits, _entered) = held_sequencer(label);
        let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
        stage_ticks(&mut seq, batches);
        let (_tx, mut rx) = mpsc::channel(4);
        let before = timeout(MAX_SYNC_HOLD - Duration::from_millis(1), seq.next_request(&mut rx)).await;
        assert!(before.is_err() && alerts.try_recv().is_err(), "{batches} pending: no page before the bound");
        let at = timeout(Duration::from_millis(2), seq.next_request(&mut rx)).await;
        assert!(at.is_err(), "nothing is taken behind the held sync");
        let page = alerts.try_recv().unwrap_or_else(|_| panic!("{batches} pending: the deadline pages"));
        assert_eq!(page.get("alert").map(String::as_str), Some("log_sync_held"));
    }
}

#[tokio::test(start_paused = true)]
async fn a_settling_sequencer_pages_a_hold_at_the_deadline() {
    // A handover stop or a shutdown drains the pipeline: that wait pages a hold too.
    let (mut seq, _permits, _entered) = held_sequencer("pipeline-held-settle");
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    stage_ticks(&mut seq, 1);
    let before = timeout(MAX_SYNC_HOLD - Duration::from_millis(1), seq.settle_async()).await;
    assert!(before.is_err() && alerts.try_recv().is_err(), "no page before the bound");
    assert!(timeout(Duration::from_millis(2), seq.settle_async()).await.is_err(), "the sync is still held");
    let page = alerts.try_recv().expect("the settle wait pages the hold");
    assert_eq!(page.get("alert").map(String::as_str), Some("log_sync_held"));
}

#[tokio::test(start_paused = true)]
async fn the_drain_takes_no_queued_request_past_the_bound() {
    let (mut seq, _permits, _entered) = held_sequencer("pipeline-held-drain");
    stage_ticks(&mut seq, 1);
    let (tx, mut rx) = mpsc::channel(4);
    tx.send(query()).await.unwrap();
    assert!(seq.next_queued(&mut rx).is_some(), "inside the bound the batch drains what is queued");
    tokio::time::advance(MAX_SYNC_HOLD).await;
    tx.send(query()).await.unwrap();
    assert!(seq.next_queued(&mut rx).is_none(), "past it the drain ends and the request waits");
}

#[tokio::test(start_paused = true)]
async fn a_request_received_past_the_deadline_waits_for_the_held_sync() {
    // A timer can fire after its deadline, so a request can arrive first: received past the bound,
    // it is kept, nothing is taken while the sync is held, and it is handed out once the hold clears.
    let (mut seq, permits, _entered) = held_sequencer("pipeline-late-request");
    stage_ticks(&mut seq, 1);
    let deadline = tokio::time::Instant::now() + MAX_SYNC_HOLD;
    tokio::time::advance(MAX_SYNC_HOLD).await;
    assert!(seq.admit(query(), deadline).is_none(), "past the deadline the request is kept, not stamped");
    let (_tx, mut rx) = mpsc::channel(4);
    let held = timeout(MAX_SYNC_HOLD, seq.next_request(&mut rx)).await;
    assert!(held.is_err(), "the kept request waits behind the held sync");
    tokio::time::resume();
    permits.send(Verdict::Pass).unwrap();
    let taken = timeout(Duration::from_secs(10), seq.next_request(&mut rx)).await.expect("the hold clears");
    assert!(matches!(taken.unwrap(), Some(Request::Query(_))), "the kept request is handed out");
}

#[tokio::test(start_paused = true)]
async fn a_sync_that_finished_inside_the_bound_is_no_hold() {
    // The sync finishes before the deadline, but the sequencer looks only after it: the finished
    // sync is released first, so nothing pages.
    let (mut seq, permits, _entered) = held_sequencer("pipeline-finished-early");
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    stage_ticks(&mut seq, 1);
    permits.send(Verdict::Pass).unwrap();
    let pipeline = seq.pipeline.as_ref().expect("the pipeline runs");
    while pipeline.shared.done.lock().unwrap().is_empty() {
        std::thread::yield_now();
    }
    tokio::time::advance(MAX_SYNC_HOLD).await;
    let (tx, mut rx) = mpsc::channel(4);
    tx.send(query()).await.unwrap();
    let taken = timeout(Duration::from_secs(1), seq.next_request(&mut rx)).await.expect("nothing is held");
    assert!(matches!(taken.unwrap(), Some(Request::Query(_))));
    assert!(alerts.try_recv().is_err(), "a finished sync never pages");
}

#[tokio::test(start_paused = true)]
async fn a_restart_resolves_an_episode_an_earlier_run_left_open() {
    // The sequencer's first look at the pipeline, here the flush its setup makes, finds nothing
    // held and resolves whatever episode the router restored.
    let (_guard, _alerts, mut resolved) = capture_alerts_and_resolutions();
    let (_seq, _permits, _entered) = held_sequencer("pipeline-held-restart");
    let end = resolved.try_recv().expect("the first look with nothing held resolves a leftover episode");
    assert_eq!(end.get("alert_resolved").map(String::as_str), Some("log_sync_held"));
}

// ── Helpers ──

/// A sequencer past genesis and trusted time whose log syncs each wait for a verdict the test sends,
/// with one message on the returned receiver each time the thread starts a sync.
pub(super) fn held_sequencer(label: &str) -> (Sequencer, StdSender<Verdict>, StdReceiver<()>) {
    let dir = scratch_dir(label);
    let (stream, _) = broadcast::channel(256);
    let (withdrawals, _custody) = mpsc::unbounded_channel();
    let clock = Arc::new(crate::clock::test_clock::TestClock::default());
    let cfg = crate::sequencer::SequencerConfig {
        clock: clock.clone(),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(cfg, Arc::new(crate::tee::EnclaveKey::test_key()), stream, withdrawals).unwrap();
    let (permits, verdicts) = channel();
    let verdicts = Mutex::new(verdicts);
    let (started, entered) = channel();
    seq.pipeline_with_syncs(move |job| {
        let _ = started.send(());
        match verdicts.lock().unwrap().recv() {
            Ok(Verdict::Pass) => job.sync(),
            _ => anyhow::bail!("the disk failed"),
        }
    })
    .unwrap();
    let weighting = dev_config().genesis.params.toxicity.notional.expect("the dev config weighs by notional");
    seq.carry_toxicity_notional(weighting).unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    (seq, permits, entered)
}

/// Stages `batches` one-tick batches behind the held sync, as `run` would.
fn stage_ticks(seq: &mut Sequencer, batches: usize) {
    for _ in 0..batches {
        seq.sequence(Command::Tick).unwrap().unwrap();
        let release = seq.stage(Batch::default()).unwrap();
        seq.submit(release).unwrap();
    }
}

fn query() -> Request {
    Request::Query(Box::new(|_| {}))
}
