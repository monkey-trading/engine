//! The held-sync bound: past `MAX_SYNC_HOLD` the sequencer takes no request, drained or new, and
//! pages Critical `log_sync_held` once — from any wait on the sync, with nothing arriving — and the
//! episode resolves once no batch in flight is past the bound, an earlier run's included. A batch
//! released from any of those waits counts as the thread's busy time.

use std::future::Future;
use std::sync::mpsc::{Receiver as StdReceiver, Sender as StdSender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_core::command::Command;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::timeout;
use tracing_subscriber::layer::SubscriberExt;

use crate::clock::test_clock::TestClock;
use crate::log::Manifest;
use crate::sequencer::pipeline::Batch;
use crate::sequencer::pipeline::hold::MAX_SYNC_HOLD;
use crate::sequencer::pipeline::tests::Verdict;
use crate::sequencer::{Request, Sequencer};
use crate::test_support::{capture_alerts_and_resolutions, dev_config, sample_around, scratch_dir, sequencer_config};

/// How long, in real time, a test waits for the sync thread to start or report a sync.
const SYNC_WAIT: Duration = Duration::from_secs(10);

#[tokio::test(start_paused = true)]
async fn a_sync_held_past_the_bound_pages_once_and_stamps_nothing_until_it_finishes() {
    let (mut seq, permits, entered) = held_sequencer("pipeline-held");
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    stage_ticks(&mut seq, 1);
    entered.recv_timeout(SYNC_WAIT).expect("the thread starts the first sync");
    let (tx, mut rx) = mpsc::channel(4);
    // Inside the bound the next request is taken while the sync runs.
    tx.send(query()).await.unwrap();
    let taken = timeout(Duration::from_secs(1), seq.next_request(&mut rx)).await.expect("inside the bound");
    assert!(matches!(taken.unwrap(), Some(Request::Query(_))));
    assert!(alerts.try_recv().is_err(), "no page inside the bound");
    // The first look resolved whatever episode an earlier run left open; that is not this hold's.
    while resolved.try_recv().is_ok() {}
    // Past it, the request waits behind the held sync, and the hold pages once however often the
    // sequencer looks; the gauge reports it from memory.
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
    assert_eq!(seq.metrics.log_sync_held.get(), 1, "the gauge reports the hold");
    assert!(resolved.try_recv().is_err(), "nothing resolves while the sync is held");
    // A batch staged behind the held one, while the thread is inside the first sync, is a job of its
    // own and inside its own bound: once the held sync finishes, the episode ends while that batch's
    // sync is still waiting for its verdict. The sync finishes on the real disk, so real time resumes.
    tokio::time::resume();
    stage_ticks(&mut seq, 1);
    permits.send(Verdict::Pass).unwrap();
    entered.recv_timeout(SYNC_WAIT).expect("the thread starts the successor's sync on its own");
    let taken = timeout(Duration::from_secs(10), seq.next_request(&mut rx)).await.expect("a finished sync frees it");
    assert!(matches!(taken.unwrap(), Some(Request::Query(_))));
    let end = resolved.try_recv().expect("the finished sync resolves the hold");
    assert_eq!(end.get("alert_resolved").map(String::as_str), Some("log_sync_held"));
    assert!(resolved.try_recv().is_err(), "one resolution for the hold");
    assert_eq!(seq.metrics.log_sync_held.get(), 0, "the gauge clears with the hold");
    assert_eq!(seq.pipeline.as_ref().unwrap().in_flight.len(), 1, "the successor's sync is still pending");
    assert!(alerts.try_recv().is_err(), "the release does not page again");
}

#[tokio::test(start_paused = true)]
async fn a_quiet_node_pages_a_hold_at_the_deadline() {
    // Nothing arrives: the wait itself wakes at the bound, with one batch pending and with the
    // pipeline full.
    for (label, batches) in [("pipeline-quiet-one", 1), ("pipeline-quiet-full", 64)] {
        let (mut seq, _permits, _entered) = held_sequencer(label);
        let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
        stage_ticks(&mut seq, batches);
        if batches > 1 {
            // The pipeline full: the batches waiting charged to its bound.
            let pipeline = seq.pipeline.as_mut().unwrap();
            pipeline.bound = pipeline.charged;
        }
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
    assert!(seq.pipeline.as_ref().unwrap().kept.is_none(), "a request never received is not kept");
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
async fn a_kept_request_waits_when_a_younger_batch_crosses_the_bound_while_the_resolution_is_written() {
    // Batch A's sync is held past the bound with a request kept, and a younger batch B is in
    // flight. A's sync finishes, B is inside its bound, and the hold's resolution is logged — a
    // write the host blocks until B is past its own bound. The kept request is judged again and
    // waits behind B's held sync, which pages.
    let (mut seq, permits, entered) = held_sequencer("pipeline-kept-rejudged");
    stage_ticks(&mut seq, 1);
    let deadline = tokio::time::Instant::now() + MAX_SYNC_HOLD;
    // B is staged only once the thread is inside A's sync, so B's sync is a job of its own: a thread
    // that took A only after B was queued would report A only with B.
    entered.recv_timeout(SYNC_WAIT).expect("the thread starts A's sync");
    tokio::time::advance(Duration::from_secs(1)).await;
    stage_ticks(&mut seq, 1);
    tokio::time::advance(MAX_SYNC_HOLD - Duration::from_secs(1)).await;
    assert!(seq.admit(query(), deadline).is_none(), "past A's deadline the request is kept");
    seq.pipeline.as_mut().unwrap().held = true;
    permits.send(Verdict::Pass).unwrap();
    await_reported(&seq, 1);
    crate::test_tracing::keep_callsites_interested();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let layer = BlockedResolution { by: Duration::from_secs(2), seen: seen.clone() };
    let _guard = tracing::subscriber::set_default(tracing_subscriber::Registry::default().with(layer));
    let (_tx, mut rx) = mpsc::channel(4);
    let taken = timeout(Duration::from_secs(1), seq.next_request(&mut rx)).await;
    assert!(taken.is_err(), "the kept request is not stamped behind B's held sync");
    assert!(seq.pipeline.as_ref().unwrap().kept.is_some(), "the request stays kept");
    assert_eq!(*seen.lock().unwrap(), ["resolved log_sync_held", "log_sync_held"], "B's hold pages");
}

#[tokio::test(start_paused = true)]
async fn a_sync_that_finished_inside_the_bound_is_no_hold() {
    // The sync finishes before the deadline, but the sequencer looks only after it: the finished
    // sync is released first, so nothing pages.
    let (mut seq, permits, _entered) = held_sequencer("pipeline-finished-early");
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    stage_ticks(&mut seq, 1);
    permits.send(Verdict::Pass).unwrap();
    await_reported(&seq, 1);
    tokio::time::advance(MAX_SYNC_HOLD).await;
    let (tx, mut rx) = mpsc::channel(4);
    tx.send(query()).await.unwrap();
    let taken = timeout(Duration::from_secs(1), seq.next_request(&mut rx)).await.expect("nothing is held");
    assert!(matches!(taken.unwrap(), Some(Request::Query(_))));
    assert!(alerts.try_recv().is_err(), "a finished sync never pages");
}

#[tokio::test(start_paused = true)]
async fn a_restart_resolves_an_episode_an_earlier_run_left_open() {
    // The running sequencer's first look at the pipeline finds nothing held and resolves whatever
    // episode the router restored.
    let (_guard, _alerts, mut resolved) = capture_alerts_and_resolutions();
    let (mut seq, _permits, _entered) = held_sequencer("pipeline-held-restart");
    let (tx, mut rx) = mpsc::channel(4);
    tx.send(query()).await.unwrap();
    let taken = timeout(Duration::from_secs(1), seq.next_request(&mut rx)).await.expect("nothing is in flight");
    assert!(matches!(taken.unwrap(), Some(Request::Query(_))));
    let end = resolved.try_recv().expect("the first look with nothing held resolves a leftover episode");
    assert_eq!(end.get("alert_resolved").map(String::as_str), Some("log_sync_held"));
}

#[tokio::test]
async fn a_release_between_batches_counts_as_the_threads_busy_time() {
    // Each wait releases a finished sync: found at the look, while waiting for a request, or while
    // the pipeline is full. The release's own work, here 7 ms of the sequencer's clock, is busy time.
    for (label, wait) in [("busy-at-look", Wait::Look), ("busy-for-request", Wait::Request), ("busy-full", Wait::Full)]
    {
        let clock = Arc::new(TestClock::default());
        let (mut seq, permits, entered) = held_sequencer_on(label, clock.clone());
        seq.sequence(Command::Tick).unwrap().unwrap();
        let mut batch = Batch::default();
        batch.later.push(Box::new(move || clock.advance(Duration::from_millis(7))));
        let release = seq.stage(batch).unwrap();
        seq.submit(release).unwrap();
        entered.recv_timeout(SYNC_WAIT).expect("the thread starts the sync");
        if wait == Wait::Full {
            let pipeline = seq.pipeline.as_mut().unwrap();
            pipeline.bound = pipeline.charged;
        }
        let (busy, shared) =
            (seq.metrics.latency.sequencer_busy.clone(), seq.pipeline.as_ref().unwrap().shared.clone());
        let spent = busy.get();
        let (tx, mut rx) = mpsc::channel(4);
        if wait == Wait::Look {
            permits.send(Verdict::Pass).unwrap();
            await_reported(&seq, 1);
        }
        let mut next = std::pin::pin!(seq.next_request(&mut rx));
        if wait != Wait::Look {
            let first = std::future::poll_fn(|cx| std::task::Poll::Ready(next.as_mut().poll(cx))).await;
            assert!(first.is_pending(), "{label}: the sequencer waits with the sync held");
            permits.send(Verdict::Pass).unwrap();
            let done = shared.done.lock().unwrap();
            let (done, waited) = shared.ready.wait_timeout_while(done, SYNC_WAIT, |done| done.is_empty()).unwrap();
            drop(done);
            assert!(!waited.timed_out(), "{label}: the sync thread reports the sync");
            // The sync thread wakes the wait just after it reports, so a request sent at once could be
            // taken first: the wait is polled, with no request, until it has taken the release.
            let released = tokio::time::Instant::now() + SYNC_WAIT;
            while !shared.done.lock().unwrap().is_empty() {
                assert!(tokio::time::Instant::now() < released, "{label}: the wait takes the release");
                let polled = std::future::poll_fn(|cx| std::task::Poll::Ready(next.as_mut().poll(cx))).await;
                assert!(polled.is_pending(), "{label}: nothing is taken with no request");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        tx.send(query()).await.unwrap();
        let taken = timeout(SYNC_WAIT, next).await.expect("the sync finished");
        assert!(matches!(taken.unwrap(), Some(Request::Query(_))));
        let counted = busy.get() - spent;
        assert!((counted - 0.007).abs() < 1e-9, "{label}: the release counts as busy time, not {counted} s");
    }
}

// ── Helpers ──

/// Where the sequencer is when a sync finishes.
#[derive(Clone, Copy, PartialEq)]
enum Wait {
    /// Before it looks for the next request.
    Look,
    /// While it waits for a request.
    Request,
    /// While it waits for the sync with the pipeline full.
    Full,
}

/// A sequencer past genesis and trusted time whose log syncs each wait for a verdict the test sends,
/// with one message on the returned receiver each time the thread starts a sync.
pub(super) fn held_sequencer(label: &str) -> (Sequencer, StdSender<Verdict>, StdReceiver<()>) {
    held_sequencer_on(label, Arc::new(TestClock::default()))
}

/// `held_sequencer`, on `clock`.
fn held_sequencer_on(label: &str, clock: Arc<TestClock>) -> (Sequencer, StdSender<Verdict>, StdReceiver<()>) {
    let dir = scratch_dir(label);
    let (stream, _) = broadcast::channel(256);
    let (withdrawals, _custody) = mpsc::unbounded_channel();
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

/// Waits until the sync thread has reported `count` finished syncs not yet taken, failing past
/// `SYNC_WAIT`.
fn await_reported(seq: &Sequencer, count: usize) {
    let shared = &seq.pipeline.as_ref().expect("the pipeline runs").shared;
    let done = shared.done.lock().unwrap();
    let (_done, wait) = shared.ready.wait_timeout_while(done, SYNC_WAIT, |done| done.len() < count).unwrap();
    assert!(!wait.timed_out(), "the sync thread reports {count} finished syncs");
}

fn query() -> Request {
    Request::Query(Box::new(|_| {}))
}

/// A host that blocks the write of the `log_sync_held` resolution for `by`: the paused clock moves
/// on while the line is written. Records every alert and resolution it sees, in order.
struct BlockedResolution {
    by: Duration,
    seen: Arc<Mutex<Vec<String>>>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for BlockedResolution {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut alert = AlertName::default();
        event.record(&mut alert);
        let Some(name) = alert.0 else { return };
        if name == "resolved log_sync_held" {
            // `advance` moves the clock on its first poll; the dropped future only yielded after.
            let mut advance = std::pin::pin!(tokio::time::advance(self.by));
            let _ = advance.as_mut().poll(&mut std::task::Context::from_waker(std::task::Waker::noop()));
        }
        self.seen.lock().unwrap().push(name);
    }
}

/// An event's `alert`, or its `alert_resolved` prefixed with `resolved`.
#[derive(Default)]
struct AlertName(Option<String>);

impl tracing::field::Visit for AlertName {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "alert" => self.0 = Some(value.to_owned()),
            "alert_resolved" => self.0 = Some(format!("resolved {value}")),
            _ => {}
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}
