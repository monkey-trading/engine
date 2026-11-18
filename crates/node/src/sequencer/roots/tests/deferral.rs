//! When a checkpoint's root waits for root work: a paced one defers while the work is over the bound
//! and is taken once it is not, or in full once the limit passes, with no request needed and with
//! requests arriving throughout; an unpaced one never defers; and a retried one still answers to the
//! gate, ending the deferral when it holds.

use super::*;
use crate::sequencer::fixture_tests::replay_trusted;

/// A bound only a backlog of several hundred leaf writes passes, as a small book reaches it.
const BOUND: u64 = 2_000;
/// Longer than any test runs: no deferral reaches it.
const NEVER: Duration = Duration::from_secs(3_600);

/// A tick whose reply arrives on the second value.
fn tick() -> (Request, oneshot::Receiver<Vec<Event>>) {
    let (reply, replied) = oneshot::channel();
    (Request::Submit { cmd: Command::Tick, reply: Some(reply) }, replied)
}

#[tokio::test]
async fn a_paced_checkpoint_defers_for_root_work_and_is_written_once_the_worker_catches_up_with_nothing_sent() {
    let mut node = Node::new("roots-deferred-caught-up", None);
    // Tens of thousands of leaf writes: the sequencer copies them out in many pieces as the worker writes.
    node.touch(12_000);
    node.seq.roots.set_bounds(BOUND, NEVER);
    let paced = node.pace_next();
    let (trigger, replied) = tick();
    let mut running = node.run(vec![trigger]);

    // The request channel stays open and nothing more is sent: only the worker's progress drives it.
    let checkpoint = running.checkpoint().await;
    assert_eq!(checkpoint.seq, paced, "the paced checkpoint is written at its own seq");
    let deferred =
        running.logged("the deferral", |logged| says(logged, "a paced checkpoint waits for root work")).await;
    assert_eq!(number(&deferred, "seq"), paced);
    assert!(number(&deferred, "wait") > BOUND, "deferred for root work over the bound: {deferred:?}");
    let committed = running.logged("the commit", |logged| says(logged, "state root committed")).await;
    assert_eq!(number(&committed, "seq"), paced);
    assert!(number(&committed, "wait") <= BOUND, "its commit waited only within the bound: {committed:?}");
    assert!(timeout(WAIT, replied).await.is_ok_and(|events| events.is_ok()), "the tick was answered");

    let Stopped { ended, entries, .. } = running.stop().await;
    ended.expect("the sequencer drains");
    let replayed = replay_trusted(&entries).expect("every checkpoint's root is the replay's");
    assert_eq!(checkpoints(&entries).last().map(|checkpoint| checkpoint.seq), Some(paced));
    assert_eq!(replayed.engine.last_seq(), Some(paced), "nothing after the paced command");
}

/// A query that sends the next as it runs, at its batch's end, so the sequencer finds a request
/// waiting whenever it looks for one, and takes no wake-up, until `stop` is set. Counts those run.
fn chained_query(tx: mpsc::Sender<Request>, stop: Arc<AtomicBool>, run: Arc<AtomicUsize>) -> Request {
    Request::Query(Box::new(move |_| {
        run.fetch_add(1, Ordering::SeqCst);
        if !stop.load(Ordering::SeqCst) {
            _ = tx.clone().try_send(chained_query(tx, stop, run));
        }
    }))
}

#[tokio::test]
async fn a_deferred_checkpoint_is_written_while_requests_keep_arriving() {
    let mut node = Node::new("roots-deferred-busy", None);
    node.touch(12_000);
    node.seq.roots.set_bounds(BOUND, NEVER);
    let paced = node.pace_next();
    let (trigger, replied) = tick();
    let mut running = node.run(vec![trigger]);
    let (stop, run) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
    running.send(chained_query(running.tx.clone(), stop.clone(), run.clone())).await;

    let checkpoint = running.checkpoint().await;
    assert_eq!(checkpoint.seq, paced, "the paced checkpoint is written at its own seq");
    assert!(run.load(Ordering::SeqCst) > 0, "the requests kept arriving meanwhile");
    stop.store(true, Ordering::SeqCst);
    let deferred =
        running.logged("the deferral", |logged| says(logged, "a paced checkpoint waits for root work")).await;
    assert_eq!(number(&deferred, "seq"), paced);
    let committed = running.logged("the commit", |logged| says(logged, "state root committed")).await;
    assert!(number(&committed, "wait") <= BOUND, "its commit waited only within the bound: {committed:?}");
    assert!(timeout(WAIT, replied).await.is_ok_and(|events| events.is_ok()), "the tick was answered");

    let Stopped { ended, entries, .. } = running.stop().await;
    ended.expect("the sequencer drains");
    replay_trusted(&entries).expect("every checkpoint's root is the replay's");
}

#[tokio::test]
async fn a_paced_checkpoint_deferred_past_its_limit_is_taken_in_full_under_root_backlog_held() {
    const LIMIT: Duration = Duration::from_millis(300);
    let mut node = Node::new("roots-deferred-held", None);
    node.touch(200);
    // No backlog is within this bound: only the limit ends the deferral.
    node.seq.roots.set_bounds(1, LIMIT);
    let paced = node.pace_next();
    let (trigger, _replied) = tick();
    let started = Instant::now();
    let mut running = node.run(vec![trigger]);

    let held = running.logged("root_backlog_held", |logged| logged.contains_key("alert")).await;
    assert_eq!((held["alert"].as_str(), held["level"].as_str()), ("root_backlog_held", "WARN"), "{held:?}");
    assert!(started.elapsed() >= LIMIT, "raised only once the limit passed");
    let committed = running.logged("the commit", |logged| says(logged, "state root committed")).await;
    assert_eq!(number(&committed, "seq"), paced);
    assert!(number(&committed, "wait") > 1, "taken in full, over the bound: {committed:?}");
    let resolved = running.logged("the resolution", |logged| logged.contains_key("alert_resolved")).await;
    assert_eq!(resolved["alert_resolved"], "root_backlog_held", "the episode ends with its commit: {resolved:?}");
    assert_eq!(running.checkpoint().await.seq, paced, "the deferred checkpoint is written");

    let Stopped { ended, entries, .. } = running.stop().await;
    ended.expect("the sequencer drains");
    replay_trusted(&entries).expect("every checkpoint's root is the replay's");
}

#[tokio::test]
async fn the_drains_final_checkpoint_is_written_whatever_the_root_work() {
    let mut node = Node::new("roots-unpaced-drain", None);
    node.touch(200);
    node.seq.roots.set_bounds(1, NEVER);
    let paced = node.pace_next();
    let (trigger, replied) = tick();
    let (after, after_replied) = tick();
    let mut running = node.run(vec![trigger, after]);
    timeout(WAIT, replied).await.expect("the paced tick is answered").expect("stamped");
    timeout(WAIT, after_replied).await.expect("the next tick is answered").expect("stamped");
    let deferred =
        running.logged("the deferral", |logged| says(logged, "a paced checkpoint waits for root work")).await;
    assert_eq!(number(&deferred, "seq"), paced, "the paced checkpoint defers");

    let Stopped { ended, entries, .. } = running.stop().await;
    ended.expect("the sequencer drains");
    let written: Vec<u64> = checkpoints(&entries).iter().map(|checkpoint| checkpoint.seq).collect();
    assert!(!written.contains(&paced), "the deferred checkpoint is never written: {written:?}");
    assert_eq!(written.last(), Some(&(paced + 1)), "the drain checkpoints the last command over the bound");
    replay_trusted(&entries).expect("every checkpoint's root is the replay's");
}

#[tokio::test]
async fn a_required_commands_checkpoint_is_written_whatever_the_root_work() {
    let mut node = Node::new("roots-unpaced-required", None);
    node.touch(200);
    let mut node = node.restart();
    // The restart has no trusted time yet, so the required command waits for the first sample.
    node.seq.sequence_required("a test's required command", vec![Command::Tick]).expect("queued");
    let required = node.seq.next_seq;
    node.seq.roots.set_bounds(1, NEVER);
    let sample = sample_around(node.clock.as_ref());
    let mut running = node.run(Vec::new());
    running.send(Request::TimeSample(sample)).await;
    let (reply, synced) = oneshot::channel();
    running.send(Request::Sync(reply)).await;
    assert_eq!(timeout(WAIT, synced).await.expect("the sync is answered").expect("synced"), Some(required));

    let committed = running.logged("the commit", |logged| says(logged, "state root committed")).await;
    assert_eq!(number(&committed, "seq"), required);
    assert!(number(&committed, "wait") > 1, "taken over the bound: {committed:?}");
    let entries = log_entries(&running.dir);
    assert_eq!(checkpoints(&entries).last().map(|checkpoint| checkpoint.seq), Some(required), "written at once");
    let Stopped { ended, entries, .. } = running.stop().await;
    ended.expect("the sequencer drains");
    replay_trusted(&entries).expect("every checkpoint's root is the replay's");
}

#[tokio::test]
async fn a_due_deferral_the_gate_withholds_writes_nothing_and_ends() {
    let (work, mut queued) = mpsc::channel(64);
    let mut node = Node::new("roots-retry-withheld", Some(work));
    while queued.try_recv().is_ok() {}
    node.touch(2_000);
    node.seq.roots.set_bounds(BOUND, NEVER);
    let paced = node.pace_next();
    let (trigger, _replied) = tick();
    // Collateral diverges right behind the paced command, before its root work is done.
    let diverged =
        Request::CheckpointStatus { posted: None, latest: None, health: Some(Health::Diverged), lifecycle: None };
    let mut running = node.run(vec![trigger, diverged]);

    let deferred =
        running.logged("the deferral", |logged| says(logged, "a paced checkpoint waits for root work")).await;
    assert_eq!(number(&deferred, "seq"), paced);
    let withheld = running.logged("the withheld retry", |logged| logged.contains_key("alert")).await;
    assert_eq!(withheld["alert"], "checkpoint_withheld", "{withheld:?}");
    assert_eq!(withheld["cause"], "collateral", "the retry answers to the gate: {withheld:?}");
    assert_eq!(running.batches_while_idle().await, 0, "the deferral ended: the sequencer idles");
    assert!(queued.try_recv().is_err(), "no checkpoint is queued for posting");

    let Stopped { ended, entries, .. } = running.stop().await;
    ended.expect("the sequencer drains");
    assert!(checkpoints(&entries).iter().all(|checkpoint| checkpoint.seq < paced), "nothing is written past the gate");
}

#[tokio::test]
async fn a_due_deferral_a_full_posting_queue_refuses_writes_nothing_and_ends() {
    let (work, mut queued) = mpsc::channel(64);
    let mut node = Node::new("roots-retry-queue-full", Some(work.clone()));
    // The poster stops taking checkpoints: its queue fills.
    let posted = std::iter::from_fn(|| queued.try_recv().ok()).last().expect("the setup's checkpoint is queued");
    while work.try_send(posted.clone()).is_ok() {}
    node.touch(2_000);
    node.seq.roots.set_bounds(BOUND, NEVER);
    let paced = node.pace_next();
    let (trigger, _replied) = tick();
    let mut running = node.run(vec![trigger]);

    let deferred =
        running.logged("the deferral", |logged| says(logged, "a paced checkpoint waits for root work")).await;
    assert_eq!(number(&deferred, "seq"), paced);
    let full = running.logged("the full queue", |logged| logged.contains_key("alert")).await;
    assert_eq!(full["alert"], "checkpoint_backlog", "the retry finds the queue full: {full:?}");
    assert_eq!(running.batches_while_idle().await, 0, "the deferral ended: the sequencer idles");

    let Stopped { ended, entries, .. } = running.stop().await;
    assert!(ended.is_ok(), "the sequencer drains: {ended:?}");
    assert!(checkpoints(&entries).iter().all(|checkpoint| checkpoint.seq < paced), "nothing is written past the queue");
}

#[tokio::test]
async fn root_backlog_held_on_a_retry_a_full_posting_queue_refuses_ends_at_the_next_commit() {
    let (work, mut queued) = mpsc::channel(64);
    let mut node = Node::new("roots-held-queue-full", Some(work.clone()));
    let posted = std::iter::from_fn(|| queued.try_recv().ok()).last().expect("the setup's checkpoint is queued");
    while work.try_send(posted.clone()).is_ok() {}
    node.touch(200);
    node.seq.roots.set_bounds(1, Duration::from_millis(300));
    let paced = node.pace_next();
    let (trigger, _replied) = tick();
    let mut running = node.run(vec![trigger]);

    let held = running.logged("root_backlog_held", |logged| logged.contains_key("alert")).await;
    assert_eq!(held["alert"], "root_backlog_held", "{held:?}");
    let full = running.logged("the full queue", |logged| logged.contains_key("alert")).await;
    assert_eq!(full["alert"], "checkpoint_backlog", "the retry taken in full finds the queue full: {full:?}");
    assert_eq!(running.batches_while_idle().await, 0, "the deferral ended: the sequencer idles");

    // The poster catches up, and the drain's checkpoint is the next commit.
    while queued.try_recv().is_ok() {}
    let Stopped { ended, entries, logged } = running.stop().await;
    ended.expect("the sequencer drains");
    assert_eq!(checkpoints(&entries).last().map(|checkpoint| checkpoint.seq), Some(paced));
    let resolved =
        logged.iter().find(|line| line.get("alert_resolved").is_some_and(|name| name == "root_backlog_held"));
    assert!(resolved.is_some(), "the episode ends with the next commit: {logged:?}");
}
