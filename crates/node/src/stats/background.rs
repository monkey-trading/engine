//! The scan of every account off the sequencer's thread. A worker thread holds a copy of the engine
//! and applies every command the sequencer applies, in order, so when the command that makes a
//! scan due has been applied there, the worker's engine is the sequencer's at that seq and the scan
//! reads exactly it. The sequencer pays one command clone per command, and a scan request; nothing
//! walks the accounts on its thread. The result is installed by the next command or read that finds
//! it, with the seq and time of the command it was due at.
//!
//! The copy is also what the API's views read (`Reader`): it applies a batch's commands only once
//! the batch is durable in the log, and a read queues behind them, so a read sees every command
//! whose reply its caller has had and none that a crash could take back.
//!
//! The worker is as deterministic as replay: `Engine::apply` is its only mutator, and it is fed the
//! commands the log holds. If it stops, or its copy would differ, the record scans on the
//! sequencer's thread as it did before, and an alert says so.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, channel, sync_channel};

use engine_core::Engine;
use engine_core::command::Sequenced;
use engine_core::types::Millis;
use tracing::warn;

use crate::stats::snapshot::{self, Layout, Snapshot};

/// Commands the worker may lag behind by before the sequencer waits for it: far more than arrive
/// during one scan, so the wait is a bound on memory and not part of the normal path.
const QUEUE: usize = 100_000;

/// One step of the worker's queue, in the order the sequencer produced it.
pub(crate) enum Job {
    Apply(Box<Sequenced>),
    Scan { seq: u64, ts: Millis, layout: Layout, full: bool },
    Read(ReadJob),
}

/// A read of the worker's copy of the engine.
pub(crate) type ReadJob = Box<dyn FnOnce(&Engine) + Send>;

/// A handle that queues reads behind the commands already handed to the worker.
#[derive(Clone)]
pub(crate) struct Reader {
    jobs: SyncSender<Job>,
    /// Commands handed over, shared with the worker's handle.
    sent: Arc<AtomicU64>,
}

impl Reader {
    /// Hands the worker a job, behind those already queued, waiting while its queue is full. A read
    /// the worker runs takes the record's lock, so the caller must not hold it. `false` once the
    /// worker has stopped.
    pub(super) fn send(&self, job: Job) -> bool {
        let command = matches!(job, Job::Apply(_));
        let sent = self.jobs.send(job).is_ok();
        self.sent.fetch_add(u64::from(sent && command), Ordering::Relaxed);
        sent
    }

    /// Queues `read` to run on the worker's copy, after every command delivered so far. Hands it
    /// back when the worker has stopped or its queue is full, for the caller to run elsewhere.
    pub(crate) fn read(&self, read: ReadJob) -> Result<(), ReadJob> {
        match self.jobs.try_send(Job::Read(read)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(Job::Read(read)) | TrySendError::Disconnected(Job::Read(read))) => Err(read),
            Err(_) => unreachable!("only a read was sent"),
        }
    }
}

pub(super) enum Done {
    Scan(Box<Snapshot>),
    Failed(String),
}

/// The sequencer's end of the worker.
pub(super) struct Background {
    jobs: SyncSender<Job>,
    done: Receiver<Done>,
    /// Scans requested whose results are not yet taken.
    in_flight: usize,
    /// Commands handed over, and commands the worker has applied.
    sent: Arc<AtomicU64>,
    #[cfg(test)]
    applied: Arc<AtomicU64>,
}

impl Background {
    /// Starts a worker over a copy of `engine`. The copy is encoded here and decoded on the worker's
    /// thread, so a node that recovers a large state does not wait for it.
    /// Error cases:
    /// - the engine cannot be encoded
    /// - the thread cannot start
    pub(super) fn start(engine: &Engine) -> anyhow::Result<Self> {
        let state = serde_json::to_vec(engine)?;
        let (jobs, work) = sync_channel(QUEUE);
        let (reply, done) = channel();
        let applied = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&applied);
        std::thread::Builder::new().name("stats-scan".into()).spawn(move || run(state, &work, &reply, &counter))?;
        Ok(Self {
            jobs,
            done,
            in_flight: 0,
            sent: Arc::default(),
            #[cfg(test)]
            applied,
        })
    }

    /// A handle for queueing jobs and reads.
    pub(super) fn reader(&self) -> Reader {
        Reader { jobs: self.jobs.clone(), sent: Arc::clone(&self.sent) }
    }

    /// Commands handed over that the worker has not applied yet.
    #[cfg(test)]
    pub(super) fn behind(&self) -> u64 {
        self.sent.load(Ordering::Relaxed) - self.applied.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// The job that asks for a scan, full or light, of the state after the command at `seq`, to queue
    /// behind that command. Every due scan is asked for, however many are still running, so what is
    /// sampled does not depend on how fast the worker is.
    pub(super) fn scan_job(&mut self, seq: u64, ts: Millis, layout: Layout, full: bool) -> Job {
        self.in_flight += 1;
        Job::Scan { seq, ts, layout, full }
    }

    /// The finished scan, if there is one. An `Err` says the worker is gone.
    pub(super) fn poll(&mut self) -> Result<Option<Box<Snapshot>>, String> {
        match self.done.try_recv() {
            Ok(Done::Scan(snapshot)) => {
                self.in_flight -= 1;
                Ok(Some(snapshot))
            }
            Ok(Done::Failed(reason)) => Err(reason),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err("the worker thread ended".into()),
        }
    }

    /// Waits for the worker's next answer. For tests, which must not sleep to synchronise.
    #[cfg(test)]
    pub(super) fn wait(&mut self) -> Result<Box<Snapshot>, String> {
        match self.done.recv() {
            Ok(Done::Scan(snapshot)) => {
                self.in_flight -= 1;
                Ok(snapshot)
            }
            Ok(Done::Failed(reason)) => Err(reason),
            Err(_) => Err("the worker thread ended".into()),
        }
    }
}

fn run(state: Vec<u8>, work: &Receiver<Job>, reply: &std::sync::mpsc::Sender<Done>, applied: &AtomicU64) {
    let decoded = serde_json::from_slice(&state);
    // The encoding is as large as the engine; it is not kept for the life of the thread.
    drop(state);
    let mut engine: Engine = match decoded {
        Ok(engine) => engine,
        Err(error) => {
            let _ = reply.send(Done::Failed(format!("decoding the engine copy: {error}")));
            return;
        }
    };
    for job in work {
        match job {
            Job::Apply(sequenced) => {
                if let Err(error) = engine.apply(&sequenced) {
                    let _ = reply.send(Done::Failed(format!("applying seq {}: {error}", sequenced.seq)));
                    return;
                }
                applied.fetch_add(1, Ordering::Relaxed);
            }
            Job::Read(read) => read(&engine),
            Job::Scan { seq, ts, layout, full } => {
                if engine.last_seq() != Some(seq) {
                    let reason = format!("the copy is at {:?}, not seq {seq}", engine.last_seq());
                    let _ = reply.send(Done::Failed(reason));
                    return;
                }
                let snapshot = snapshot::scan(&engine, seq, ts, layout, full);
                if reply.send(Done::Scan(Box::new(snapshot))).is_err() {
                    return;
                }
            }
        }
    }
}

/// The alert for a worker that stopped: the record scans on the sequencer's thread from here.
pub(super) fn stopped(reason: &str) {
    warn!(alert = "stats_scan_inline", "the background stats scan stopped, so scans run on the sequencer: {reason}");
}
