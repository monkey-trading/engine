//! A batch leaves the sequencer only once its sync is done, the next one is sequenced meanwhile, and
//! a failed sync stops the node. The sync is a gate the test opens, so no test sleeps to order events.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver as StdReceiver, Sender as StdSender, channel};
use std::time::Duration;

use engine_core::command::Command;
use engine_core::event::Event;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::*;
use crate::log::Manifest;
use crate::test_support::{dev_config, sample_around, scratch_dir, sequencer_config};

/// What a sync does when the test lets it.
enum Verdict {
    Pass,
    Fail,
}

/// A running sequencer whose syncs wait for the test.
struct Gated {
    tx: mpsc::Sender<Request>,
    permits: StdSender<Verdict>,
    stream: broadcast::Receiver<Arc<StreamMsg>>,
    stats: crate::stats::Stats,
    /// Syncs the thread has started.
    syncs: Arc<AtomicUsize>,
    /// One message each time the thread starts a sync, before it waits for its verdict.
    entered: StdReceiver<()>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Gated {
    fn start(name: &str) -> Self {
        let dir = scratch_dir(name);
        let (stream, stream_rx) = broadcast::channel(256);
        let (withdrawals, _custody) = mpsc::unbounded_channel();
        let clock = Arc::new(crate::clock::test_clock::TestClock::default());
        let cfg = crate::sequencer::SequencerConfig {
            clock: clock.clone(),
            ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
        };
        let mut seq =
            Sequencer::recover(cfg, Arc::new(crate::tee::EnclaveKey::test_key()), stream, withdrawals).unwrap();
        let stats = crate::test_support::stats();
        seq.track_stats(stats.clone());
        let (permits, verdicts): (StdSender<Verdict>, StdReceiver<Verdict>) = channel();
        let verdicts = Mutex::new(verdicts);
        let syncs = Arc::new(AtomicUsize::new(0));
        let counted = syncs.clone();
        let (entering, entered) = channel();
        seq.pipeline_with_syncs(move |job| {
            _ = entering.send(());
            let verdict = verdicts.lock().unwrap().recv();
            match verdict {
                Ok(Verdict::Pass) => {
                    counted.fetch_add(1, Ordering::SeqCst);
                    job.sync()
                }
                _ => anyhow::bail!("the disk failed"),
            }
        })
        .unwrap();
        let weighting = dev_config().genesis.params.toxicity.notional.expect("the dev config weighs by notional");
        seq.carry_toxicity_notional(weighting).unwrap();
        seq.activate_protocol().unwrap();
        seq.genesis(&dev_config().genesis).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(seq.run(rx));
        Self { tx, permits, stream: stream_rx, stats, syncs, entered, task }
    }

    /// Waits until the sync thread has started a sync and is waiting for its verdict.
    fn sync_started(&self) {
        self.entered.recv_timeout(Duration::from_secs(10)).expect("the sync thread starts a sync");
    }

    /// Submits a tick and returns its reply.
    async fn tick(&self) -> oneshot::Receiver<Vec<Event>> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).await.unwrap();
        rx
    }

    /// The last seq the sequencer has applied, durable or not.
    async fn applied(&self) -> Option<u64> {
        let (reply, rx) = oneshot::channel();
        let query: crate::sequencer::Query = Box::new(move |engine| {
            let _ = reply.send(engine.last_seq());
        });
        self.tx.send(Request::Query(query)).await.unwrap();
        rx.await.unwrap()
    }

    /// The last seq the stats worker's copy has applied, as a view reads it.
    async fn viewed(&self) -> Option<u64> {
        let (reply, rx) = oneshot::channel();
        let reader = self.stats.reader().expect("the worker runs");
        assert!(reader.read(Box::new(move |engine| _ = reply.send(engine.last_seq()))).is_ok());
        timeout(Duration::from_secs(10), rx).await.expect("the worker answers").unwrap()
    }
}

/// Whether `rx` has nothing yet, after the sequencer has had every chance to answer.
async fn silent<T>(rx: &mut oneshot::Receiver<T>) -> bool {
    timeout(Duration::from_millis(150), rx).await.is_err()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_is_released_only_after_its_sync_and_the_next_is_sequenced_meanwhile() {
    let node = Gated::start("pipeline-gated");
    // The genesis commands reached storage in line, before the loop ran.
    let before = node.applied().await;
    let mut first = node.tick().await;
    let applied = node.applied().await;
    assert!(applied > before, "the first tick is sequenced");
    assert!(silent(&mut first).await, "its reply waits for the sync");
    assert_eq!(node.viewed().await, before, "a view does not show it either");
    let mut stream = node.stream.resubscribe();

    // A second tick is sequenced while the first batch's sync is held.
    let mut second = node.tick().await;
    assert!(node.applied().await > applied, "the sequencer does not wait for the sync");
    assert!(silent(&mut second).await);
    assert!(stream.try_recv().is_err(), "nothing of either batch is on the stream");

    node.permits.send(Verdict::Pass).unwrap();
    first.await.expect("the first batch is released by its own sync");
    assert_eq!(node.viewed().await, applied, "a view sees the durable batch and not the next");
    assert!(silent(&mut second).await, "the second batch still waits for its own sync");
    node.permits.send(Verdict::Pass).unwrap();
    second.await.expect("the second batch is released in turn");
    let seqs: Vec<u64> = std::iter::from_fn(|| stream.try_recv().ok()).map(|msg| msg.offset.seq).collect();
    assert!(seqs.windows(2).all(|pair| pair[0] <= pair[1]), "the stream keeps log order: {seqs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_sync_releases_nothing_and_stops_the_sequencer() {
    let node = Gated::start("pipeline-failed");
    let reply = node.tick().await;
    node.permits.send(Verdict::Fail).unwrap();
    let stopped = timeout(Duration::from_secs(10), node.task).await.expect("the sequencer stops").unwrap();
    let error = format!("{:#}", stopped.expect_err("a failed sync is an error"));
    assert!(error.contains("syncing the log"), "{error}");
    assert!(reply.await.is_err(), "the reply of the undurable batch is dropped, not sent");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_pipeline_takes_no_request_until_a_sync_is_done() {
    let dir = scratch_dir("pipeline-bounded");
    let (stream, _) = broadcast::channel(256);
    let (withdrawals, _custody) = mpsc::unbounded_channel();
    let clock = Arc::new(crate::clock::test_clock::TestClock::default());
    let cfg = crate::sequencer::SequencerConfig {
        clock: clock.clone(),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(cfg, Arc::new(crate::tee::EnclaveKey::test_key()), stream, withdrawals).unwrap();
    let (permits, verdicts): (StdSender<Verdict>, StdReceiver<Verdict>) = channel();
    let verdicts = Mutex::new(verdicts);
    seq.pipeline_with_syncs(move |job| match verdicts.lock().unwrap().recv() {
        Ok(Verdict::Pass) => job.sync(),
        _ => anyhow::bail!("the disk failed"),
    })
    .unwrap();
    let weighting = dev_config().genesis.params.toxicity.notional.expect("the dev config weighs by notional");
    seq.carry_toxicity_notional(weighting).unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    // One batch per tick, each waiting on the held sync, as `run` would stage them.
    for _ in 0..MAX_IN_FLIGHT {
        seq.sequence(Command::Tick).unwrap().unwrap();
        let release = seq.stage(Batch::default()).unwrap();
        seq.submit(release).unwrap();
    }
    let (tx, mut rx) = mpsc::channel(4);
    tx.send(Request::Query(Box::new(|_| {}))).await.unwrap();
    let held = timeout(Duration::from_millis(200), seq.next_request(&mut rx)).await;
    assert!(held.is_err(), "with {MAX_IN_FLIGHT} batches waiting, the next request waits too");
    permits.send(Verdict::Pass).unwrap();
    let taken = timeout(Duration::from_secs(10), seq.next_request(&mut rx)).await.expect("a finished sync frees it");
    assert!(matches!(taken.unwrap(), Some(Request::Query(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn batches_queued_behind_a_sync_share_the_next_one() {
    let node = Gated::start("pipeline-grouped");
    let first = node.tick().await;
    node.applied().await;
    node.sync_started();
    // The first sync is held, so these three queue behind it.
    let mut queued = Vec::new();
    for _ in 0..3 {
        queued.push(node.tick().await);
        node.applied().await;
    }
    node.permits.send(Verdict::Pass).unwrap();
    first.await.expect("the first batch is released by its own sync");
    // One more permit is all the three need: the thread syncs the last of them, which covers the rest.
    node.permits.send(Verdict::Pass).unwrap();
    for reply in queued {
        timeout(Duration::from_secs(10), reply).await.expect("released by the shared sync").unwrap();
    }
    assert_eq!(node.syncs.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_that_wrote_nothing_does_not_stand_in_for_the_sync_of_the_one_before_it() {
    let node = Gated::start("pipeline-empty-successor");
    let first = node.tick().await;
    node.applied().await;
    node.sync_started();
    // A batch that wrote, then one that wrote nothing but asks for a directory sync, both queued behind
    // the held sync.
    let written = node.tick().await;
    node.applied().await;
    let (reply, house) = oneshot::channel();
    node.tx.send(Request::Sync(reply)).await.unwrap();
    node.applied().await;
    for _ in 0..3 {
        node.permits.send(Verdict::Pass).unwrap();
    }
    first.await.unwrap();
    written.await.unwrap();
    timeout(Duration::from_secs(10), house).await.expect("the sync request is answered").unwrap();
    assert_eq!(node.syncs.load(Ordering::SeqCst), 3, "the batch that wrote has its own sync");
}

#[tokio::test(flavor = "multi_thread")]
async fn batches_that_only_answer_reads_take_no_pipeline_slot() {
    let node = Gated::start("pipeline-reads");
    let first = node.tick().await;
    node.applied().await;
    node.sync_started();
    // More read-only batches than the pipeline holds, all waiting on the one held sync.
    let mut answers = Vec::new();
    for _ in 0..=MAX_IN_FLIGHT {
        let (reply, answer) = oneshot::channel();
        node.tx.send(Request::HouseState(reply)).await.unwrap();
        timeout(Duration::from_secs(10), node.applied()).await.expect("the sequencer still takes requests");
        answers.push(answer);
    }
    // A price is still sequenced, and every answer goes out with the sync it waited for.
    let price = node.tick().await;
    timeout(Duration::from_secs(10), node.applied()).await.expect("a price is not held behind reads");
    node.permits.send(Verdict::Pass).unwrap();
    node.permits.send(Verdict::Pass).unwrap();
    first.await.unwrap();
    for answer in answers {
        timeout(Duration::from_secs(10), answer).await.expect("answered").unwrap();
    }
    price.await.unwrap();
}
