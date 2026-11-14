//! Group commit: a batch's data sync runs on its own thread, so the next price is stamped while the
//! last batch reaches storage. Total order is all a stamp needs, and the sync only has to finish
//! before anything the batch produced leaves the sequencer.
//!
//! At the end of a batch the sequencer writes its entries to the operating system, takes everything
//! the batch will release (replies, stream messages, signed withdrawals, mark reports, house reads,
//! the stats worker's jobs) and queues that behind a `SyncJob`. When the sync thread reports a job
//! done, the batch is released in the order the batches were taken: the manifest publishes it as
//! flushed first, then its messages go out. A failed sync stops the node, as an inline one does.
//! Batches that queue up while a sync runs share the next one. Without the thread
//! (`pipeline_log_syncs` not called), a batch syncs in line.
//!
//! Segment rotation defers to the same thread: at a boundary the writer flushes the full segment
//! and swaps the open file on the sequencer, and the seal — the segment's fsync, its durable
//! manifest listing, and the staged next segment's exclusive link — rides that batch's `SyncJob`,
//! publishing in `finish_sync` before anything of the new segment is released. The on-disk order
//! is what an inline seal gives: the old segment is listed before the new one takes its name.

mod hold;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Instant;

use anyhow::{Context, bail};
use engine_core::event::Event;
use tokio::sync::{Notify, oneshot, watch};
use tracing::error;

use crate::benchmarks::BenchmarkSchedule;
use crate::history::marks::MarkSample;
use crate::log::SyncJob;
use crate::sequencer::checkpoints::publish::After;
use crate::sequencer::house::HouseState;
use crate::sequencer::{Request, Sequencer, StreamBody, StreamMsg};
use crate::stats::background::Job as StatsJob;

/// Batches whose sync may be pending at once. A disk that stalls longer than that many batches
/// takes the sequencer's next request down with it, rather than letting what cannot be released
/// pile up.
const MAX_IN_FLIGHT: usize = 64;

/// The requests of a batch that wait for its entries to be durable.
#[derive(Default)]
pub(super) struct Batch {
    pub replies: Vec<(oneshot::Sender<Vec<Event>>, Vec<Event>)>,
    pub syncs: Vec<oneshot::Sender<Option<u64>>>,
    pub watches: Vec<oneshot::Sender<watch::Receiver<BenchmarkSchedule>>>,
    /// Answers read from state the batch changed, sent once it is durable.
    pub later: Vec<Box<dyn FnOnce() + Send>>,
    /// When the batch began, if it sequenced a command, for the batch latency metric.
    pub timing: Option<Instant>,
}

/// Everything a batch releases once its sync is done.
pub(super) struct Release {
    job: Arc<SyncJob>,
    batch: Batch,
    last_seq: Option<u64>,
    /// When the batch was staged: the hold bound runs from here.
    staged: tokio::time::Instant,
    /// The log head this batch made durable, captured at stage: the sync thread may finish an
    /// earlier batch after a later one is staged, so the link at finish is not this batch's.
    head: crate::log_heads::Head,
    /// The stamp of the command at `head`.
    stamp: engine_core::types::Millis,
    outbox: Vec<Arc<StreamMsg>>,
    marks: Vec<MarkSample>,
    house: Option<(Vec<oneshot::Sender<HouseState>>, HouseState)>,
    schedule: Option<BenchmarkSchedule>,
    stats: Vec<StatsJob>,
    after: Vec<After>,
}

/// The sync thread's results, in the order its jobs were queued.
struct Shared {
    done: Mutex<VecDeque<anyhow::Result<()>>>,
    ready: Condvar,
    wake: Notify,
    closed: AtomicBool,
}

impl Shared {
    /// Takes the oldest result, if any.
    fn pop(&self) -> Option<anyhow::Result<()>> {
        self.done.lock().unwrap_or_else(PoisonError::into_inner).pop_front()
    }

    /// Adds a result and wakes whoever waits.
    fn push(&self, result: anyhow::Result<()>) {
        self.done.lock().unwrap_or_else(PoisonError::into_inner).push_back(result);
        self.ready.notify_all();
        self.wake.notify_one();
    }

    /// The next result, waiting on the async runtime for it.
    /// Error cases:
    /// - the sync failed, or its thread ended
    async fn next(&self) -> anyhow::Result<()> {
        loop {
            let woken = self.wake.notified();
            if let Some(result) = self.pop() {
                return result;
            }
            if self.closed.load(Ordering::Acquire) {
                return self.pop().unwrap_or_else(|| bail!("the log sync thread ended"));
            }
            woken.await;
        }
    }

    /// The next result, blocking this thread for it.
    /// Error cases:
    /// - as `next`
    fn wait(&self) -> anyhow::Result<()> {
        let mut done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(result) = done.pop_front() {
                return result;
            }
            if self.closed.load(Ordering::Acquire) {
                bail!("the log sync thread ended");
            }
            done = self.ready.wait(done).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// The sync thread and the batches waiting on it.
pub(super) struct Pipeline {
    jobs: Sender<Arc<SyncJob>>,
    shared: Arc<Shared>,
    in_flight: VecDeque<Release>,
    /// Whether the oldest batch's sync is held past `MAX_SYNC_HOLD`: one `log_sync_held` episode.
    /// It starts set, so the first check with no hold past the bound resolves an episode an earlier
    /// run left open; the router discards a resolution with no open episode.
    held: bool,
    /// A request received past the oldest batch's deadline, kept until the hold clears.
    kept: Option<Request>,
}

impl Pipeline {
    /// Starts the sync thread. Call it from a thread the sequencer's CPU reservation does not bind,
    /// as the stats worker is.
    /// Error cases:
    /// - the thread cannot start
    pub(super) fn start() -> anyhow::Result<Self> {
        Self::start_with(SyncJob::sync)
    }

    /// `start`, with `sync` doing each job's sync: a test's way to hold one back or fail it.
    /// Error cases:
    /// - the thread cannot start
    pub(super) fn start_with(sync: impl Fn(&SyncJob) -> anyhow::Result<()> + Send + 'static) -> anyhow::Result<Self> {
        let (jobs, queue) = channel();
        let shared = Arc::new(Shared {
            done: Mutex::default(),
            ready: Condvar::new(),
            wake: Notify::new(),
            closed: AtomicBool::new(false),
        });
        let worker = shared.clone();
        std::thread::Builder::new().name("log-sync".into()).spawn(move || run_syncs(&queue, &worker, sync))?;
        Ok(Self { jobs, shared, in_flight: VecDeque::new(), held: true, kept: None })
    }
}

/// Syncs each job in turn. A failure is reported and ends the thread: nothing after it may be
/// released.
fn run_syncs(queue: &Receiver<Arc<SyncJob>>, shared: &Arc<Shared>, sync: impl Fn(&SyncJob) -> anyhow::Result<()>) {
    struct Close<'a>(&'a Shared);
    impl Drop for Close<'_> {
        fn drop(&mut self) {
            self.0.closed.store(true, Ordering::Release);
            self.0.ready.notify_all();
            self.0.wake.notify_one();
        }
    }
    let _close = Close(shared);
    while let Ok(first) = queue.recv() {
        // Group commit: one sync covers every job queued behind the one being synced, as they were
        // written to the same file before it.
        let group: Vec<Arc<SyncJob>> = std::iter::once(first).chain(queue.try_iter()).collect();
        let mut result = Ok(());
        for (index, job) in group.iter().enumerate() {
            if group.get(index + 1).is_some_and(|next| job.covered_by(next)) {
                continue;
            }
            result = sync(job);
            if result.is_err() {
                break;
            }
        }
        // A failure covers the whole group: none of it may be released, and the first result stops
        // the sequencer.
        if result.is_err() {
            shared.push(result);
            return;
        }
        for _ in &group {
            shared.push(Ok(()));
        }
    }
}

impl Release {
    /// Takes the answers of `other`, a batch that wrote nothing, to go out with this one: the writes
    /// before it are what it waited for, and this release's sync covers them.
    fn absorb(&mut self, other: Release) {
        let Release { batch, outbox, marks, house, schedule, stats, after, .. } = other;
        self.batch.replies.extend(batch.replies);
        self.batch.watches.extend(batch.watches);
        self.batch.later.extend(batch.later);
        self.outbox.extend(outbox);
        self.marks.extend(marks);
        self.stats.extend(stats);
        self.after.extend(after);
        self.schedule = schedule.or(self.schedule.take());
        self.house = match (self.house.take(), house) {
            (Some((mut waiting, state)), Some((more, _))) => {
                waiting.extend(more);
                Some((waiting, state))
            }
            (held, later) => held.or(later),
        };
    }

    /// Whether there is nothing to sync or send: a batch of reads and refusals, which needs no place
    /// in the order.
    fn is_inert(&self) -> bool {
        self.job.is_empty()
            && self.outbox.is_empty()
            && self.marks.is_empty()
            && self.house.is_none()
            && self.schedule.is_none()
            && self.stats.is_empty()
            && self.after.is_empty()
            && self.batch.replies.is_empty()
            && self.batch.syncs.is_empty()
            && self.batch.watches.is_empty()
            && self.batch.later.is_empty()
            && self.batch.timing.is_none()
    }
}

impl Sequencer {
    /// Moves each batch's data sync onto its own thread, for a sequencer that `run`s. Segment
    /// rotation's seal defers to the same thread.
    /// Error cases:
    /// - the thread cannot start
    pub fn pipeline_log_syncs(&mut self) -> anyhow::Result<()> {
        self.pipeline = Some(Pipeline::start()?);
        self.log.defer_seals();
        Ok(())
    }

    /// `pipeline_log_syncs` with `sync` doing each job's sync.
    /// Error cases:
    /// - the thread cannot start
    #[cfg(test)]
    pub(super) fn pipeline_with_syncs(
        &mut self,
        sync: impl Fn(&SyncJob) -> anyhow::Result<()> + Send + 'static,
    ) -> anyhow::Result<()> {
        self.pipeline = Some(Pipeline::start_with(sync)?);
        self.log.defer_seals();
        Ok(())
    }

    /// Writes the batch's entries to the operating system and takes what it releases once they are
    /// durable. The entries go out in the next `finish` in line.
    /// Error cases:
    /// - the buffered write fails, or a file handle cannot be duplicated
    pub(super) fn stage(&mut self, batch: Batch) -> anyhow::Result<Release> {
        let withdrawals = self.outbox.iter().any(|msg| matches!(msg.body, StreamBody::Withdrawal(_)));
        let directory = withdrawals || !batch.syncs.is_empty() || !self.after_sync.is_empty();
        let job = Arc::new(self.log.begin_sync(directory)?);
        let schedule =
            std::mem::take(&mut self.benchmark_schedule_dirty).then(|| BenchmarkSchedule::from_engine(&self.engine));
        Ok(Release {
            job,
            batch,
            last_seq: self.engine.last_seq(),
            staged: tokio::time::Instant::now(),
            head: crate::log_heads::Head { seq: self.engine.last_seq().unwrap_or(0), link: self.command_prefix.into() },
            stamp: self.engine.now(),
            outbox: std::mem::take(&mut self.outbox),
            marks: self.marks.take_pending(),
            house: self.house.take_answer(&self.engine, self.progress.params_seq),
            schedule,
            stats: self.marks.stage_stats(),
            after: std::mem::take(&mut self.after_sync),
        })
    }

    /// Releases a batch whose entries are durable: hands the stats worker the batch's commands,
    /// publishes the entries as flushed, then sends what they produced. The worker has the commands
    /// before anything a client could act on goes out, so a view read after it sees them.
    /// Error cases:
    /// - a checkpoint cannot be posted
    fn finish(&mut self, release: Release) -> anyhow::Result<()> {
        let Release { job, batch, last_seq, staged: _, head, stamp, outbox, marks, house, schedule, stats, after } =
            release;
        self.marks.deliver_stats(stats);
        self.log.finish_sync(&job);
        if let Some(publisher) = &self.log_heads
            && *publisher.tx.borrow() != head
        {
            publisher.tx.send_replace(head);
        }
        if let Some(publisher) = &self.log_heads {
            let released = crate::liveness::Released { head, stamp: u64::try_from(stamp).unwrap_or(0) };
            publisher.released.send_if_modified(|was| std::mem::replace(was, released) != released);
        }
        self.run_after(after)?;
        if let Some(schedule) = schedule {
            self.benchmark_schedule.send_replace(schedule);
        }
        self.marks.release(marks);
        if let Some((waiting, state)) = house {
            for reply in waiting {
                let _ = reply.send(state.clone());
            }
        }
        for msg in outbox {
            if let StreamBody::Withdrawal(w) = &msg.body
                && self.withdrawals.send(w.clone()).is_err()
            {
                let (seq, account, nonce) = (msg.offset.seq, w.account, w.nonce);
                error!(alert = "custody_unavailable", seq, %account, nonce, "signed withdrawal has no custody consumer");
            }
            let _ = self.stream.send(msg);
        }
        for reply in batch.syncs {
            let _ = reply.send(last_seq);
        }
        for (reply, events) in batch.replies {
            let _ = reply.send(events);
        }
        for reply in batch.watches {
            let _ = reply.send(self.benchmark_schedule.subscribe());
        }
        for answer in batch.later {
            answer();
        }
        // Only admitted commands are written: a batch of reads or refusals would dilute the latency.
        if let Some(started) = batch.timing {
            self.metrics.batch_seconds.observe(self.clock.instant().duration_since(started).as_secs_f64());
        }
        Ok(())
    }

    /// Queues `release` behind the batches already waiting for their sync, or syncs and releases it
    /// here when no thread runs the syncs or there is nothing to wait for.
    /// Error cases:
    /// - the inline sync fails, or the sync thread has stopped
    pub(super) fn submit(&mut self, release: Release) -> anyhow::Result<()> {
        if release.is_inert() {
            return Ok(());
        }
        let idle = self.pipeline.as_ref().is_none_or(|pipeline| pipeline.in_flight.is_empty());
        if self.pipeline.is_none() || (idle && release.job.is_empty()) {
            if !release.job.is_empty() {
                release.job.sync()?;
            }
            return self.finish(release);
        }
        let pipeline = self.pipeline.as_mut().expect("checked above");
        // A batch that wrote nothing needs no sync of its own: it goes out with the one before it.
        if release.job.is_empty()
            && let Some(last) = pipeline.in_flight.back_mut()
        {
            last.absorb(release);
            return Ok(());
        }
        pipeline.jobs.send(release.job.clone()).map_err(|_| anyhow::anyhow!("the log sync thread has stopped"))?;
        pipeline.in_flight.push_back(release);
        Ok(())
    }

    /// Takes the sync thread's result for the oldest batch in flight and releases the batch.
    /// Error cases:
    /// - the sync failed: the node must stop rather than acknowledge undurable commands
    fn complete(&mut self, done: anyhow::Result<()>) -> anyhow::Result<()> {
        let release = self.pipeline.as_mut().and_then(|pipeline| pipeline.in_flight.pop_front());
        let release = release.context("a log sync finished with no batch in flight")?;
        done.context("syncing the log")?;
        self.finish(release)
    }

    /// Makes everything written so far durable, then releases it: the batches in flight first, so
    /// the stream stays in log order, then what is buffered. For a caller that must not go on
    /// before that, and for the sequencer when it has no thread for the syncs.
    /// Error cases:
    /// - the log cannot be written or synced
    pub(super) fn flush(&mut self) -> anyhow::Result<()> {
        self.settle()?;
        let release = self.stage(Batch::default())?;
        release.job.sync()?;
        self.finish(release)
    }
}

#[cfg(test)]
mod hold_tests;
#[cfg(test)]
mod tests;
