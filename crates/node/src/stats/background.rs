//! The scan of every account off the sequencer's thread. A worker thread holds a copy of the engine
//! and applies every command the sequencer applies, in order, so when the command that makes a
//! scan due has been applied there, the worker's engine is the sequencer's at that seq and the scan
//! reads exactly it. The sequencer pays one command clone and a channel send per command, and a
//! scan request; nothing walks the accounts on its thread. The result is installed by the next
//! command or read that finds it, with the seq and time of the command it was due at.
//!
//! The worker is as deterministic as replay: `Engine::apply` is its only mutator, and it is fed the
//! commands the log holds. If it stops, or its copy would differ, the record scans on the
//! sequencer's thread as it did before, and an alert says so.

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, channel, sync_channel};

use engine_core::Engine;
use engine_core::command::Sequenced;
use engine_core::types::Millis;
use tracing::warn;

use crate::stats::snapshot::{self, Layout, Snapshot};

/// Commands the worker may lag behind by before the sequencer waits for it: far more than arrive
/// during one scan, so the wait is a bound on memory and not part of the normal path.
const QUEUE: usize = 100_000;

enum Job {
    Apply(Box<Sequenced>),
    Scan { seq: u64, ts: Millis, layout: Layout },
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
        std::thread::Builder::new().name("stats-scan".into()).spawn(move || run(&state, &work, &reply))?;
        Ok(Self { jobs, done, in_flight: 0 })
    }

    /// Hands the worker a command the sequencer applied. `false` once the worker has stopped.
    pub(super) fn apply(&self, sequenced: &Sequenced) -> bool {
        self.jobs.send(Job::Apply(Box::new(sequenced.clone()))).is_ok()
    }

    #[cfg(test)]
    pub(super) fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Asks for a full scan of the state after the command at `seq`. Every due scan is asked for,
    /// however many are still running, so what is sampled does not depend on how fast the worker
    /// is. `false` once the worker has stopped.
    pub(super) fn request(&mut self, seq: u64, ts: Millis, layout: Layout) -> bool {
        let sent = self.jobs.send(Job::Scan { seq, ts, layout }).is_ok();
        self.in_flight += usize::from(sent);
        sent
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

fn run(state: &[u8], work: &Receiver<Job>, reply: &std::sync::mpsc::Sender<Done>) {
    let mut engine: Engine = match serde_json::from_slice(state) {
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
            }
            Job::Scan { seq, ts, layout } => {
                if engine.last_seq() != Some(seq) {
                    let reason = format!("the copy is at {:?}, not seq {seq}", engine.last_seq());
                    let _ = reply.send(Done::Failed(reason));
                    return;
                }
                let snapshot = snapshot::scan(&engine, seq, ts, layout, true);
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
