//! A replay's state roots, computed on a thread of their own when the replay asks (`check_roots_ahead`).
//! The engine computes its roots until it holds a tree under `V9`; the checkpoint whose root first gives
//! it one hands that tree to the thread once its root is checked (`Engine::detach_tree`), as the
//! sequencer hands it to its root worker, and from then on the replay sends it each checkpoint's leaf
//! writes (`Engine::commit_inputs`), and each tree
//! event an activation gives, in log order. The thread commits each root and releases its version at
//! once, as an engine computing its own roots does, so its roots are the engine's own.
//!
//! The replay applies the next checkpoint's commands while the thread computes a root, and compares the
//! root when the next checkpoint comes, or when it settles (`settled`): before it reports an error of
//! its own or of its read, so a root that differs is reported at its checkpoint, ahead of any later
//! entry's fault, and before it finishes, when it takes the tree back into its engine
//! (`Engine::attach_tree`). At most one root is computed at a time. A replay checking its seals at the
//! end of each run (`check_run_ends`) checks the open run the same way, after the root still due.

use std::panic;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::thread::{self, JoinHandle};

use anyhow::bail;
use engine_core::Engine;
use engine_core::eip712::B256;
use engine_core::merkle::tree::StateTree;
use engine_core::merkle::{CommitInputs, TreeEvent};
use tracing::warn;

use crate::log::{LogEntry, SealChecks};
use crate::sequencer::replay::Replay;

/// Jobs the replay may send ahead of the root it waits for: a root's commit and the tree events of the
/// activations before it.
const JOBS: usize = 4;

/// How a replay computes its state roots.
#[derive(Default)]
pub(super) enum Roots {
    /// The engine computes each root at its checkpoint.
    #[default]
    Inline,
    /// The engine computes its roots until it holds a tree, which then moves to a thread.
    Ahead(Option<Thread>),
}

/// The thread holding the replay's tree, and the root it is computing.
pub(super) struct Thread {
    /// Closed to end the thread, which then returns the tree.
    jobs: Option<SyncSender<Job>>,
    roots: Receiver<B256>,
    /// The checkpoint whose root the thread is computing: its seq and the root the log gives it.
    due: Option<(u64, String)>,
    handle: Option<JoinHandle<StateTree>>,
}

/// What the replay asks of the thread, in log order.
enum Job {
    /// Commit a root's leaf writes, release its version and answer with the root.
    Commit(u64, CommitInputs),
    /// Apply an activation's tree event.
    Event(TreeEvent),
}

impl Drop for Thread {
    /// Ends the thread and joins it. A thread that panicked has had its panic raised already, or the
    /// replay is ending on an error of its own.
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Roots {
    /// Whether the tree is on the thread.
    pub(super) fn on_thread(&self) -> bool {
        matches!(self, Roots::Ahead(Some(_)))
    }

    /// Checks the root of `engine`'s state at the checkpoint at `seq` against `logged`, the root the log
    /// gives it: in place while the engine computes its roots, and otherwise once the thread has it.
    /// Error cases:
    /// - a root that differs from the log's, here or the one still due
    /// - the thread has stopped
    pub(super) fn checkpoint(&mut self, engine: &mut Engine, seq: u64, logged: &str) -> anyhow::Result<()> {
        let Roots::Ahead(ahead) = self else { return check(seq, logged, engine.state_root().into()) };
        let Some(thread) = ahead else {
            check(seq, logged, engine.state_root().into())?;
            *ahead = Thread::start(engine);
            return Ok(());
        };
        thread.check_due()?;
        thread.send(Job::Commit(seq, engine.commit_inputs()))?;
        thread.due = Some((seq, logged.to_owned()));
        Ok(())
    }

    /// Passes the tree event the last command gave, if any, to the thread.
    /// Error cases:
    /// - the thread has stopped
    pub(super) fn after_command(&mut self, engine: &mut Engine) -> anyhow::Result<()> {
        match (self, engine.take_tree_event()) {
            (Roots::Ahead(Some(thread)), Some(event)) => thread.send(Job::Event(event)),
            _ => Ok(()),
        }
    }

    /// Checks the root still due, if any.
    /// Error cases:
    /// - it differs from the log's, or the thread has stopped
    pub(super) fn check_due(&mut self) -> anyhow::Result<()> {
        match self {
            Roots::Ahead(Some(thread)) => thread.check_due(),
            _ => Ok(()),
        }
    }

    /// Checks the root still due, ends the thread and takes its tree back into `engine`, which computes
    /// its roots until the next checkpoint hands the tree to a thread again.
    /// Error cases:
    /// - the root still due differs from the log's, or the thread has stopped
    pub(super) fn take_back(&mut self, engine: &mut Engine) -> anyhow::Result<()> {
        if let Roots::Ahead(ahead @ Some(_)) = self {
            let mut thread = ahead.take().expect("matched above");
            thread.check_due()?;
            engine.attach_tree(thread.join());
        }
        Ok(())
    }
}

impl Thread {
    /// A thread holding `engine`'s tree, or `None` while the engine holds none (before its first root
    /// under `V9`) or when the thread cannot start, and the engine keeps computing its roots.
    fn start(engine: &mut Engine) -> Option<Self> {
        engine.last_root_work()?;
        let (jobs, received) = sync_channel(JOBS);
        let (answer, roots) = channel();
        let (give, tree) = sync_channel(1);
        let spawned = thread::Builder::new().name("replay-roots".into()).spawn(move || {
            // The tree comes once the thread has started, so one that cannot start leaves it with the engine.
            let tree = tree.recv().expect("the replay hands over the tree once the thread starts");
            run(tree, received, answer)
        });
        let handle = match spawned {
            Ok(handle) => handle,
            Err(e) => {
                warn!("the replay computes its roots in place: its root thread did not start: {e}");
                return None;
            }
        };
        let tree = engine.detach_tree().expect("an engine with a root under V9 holds its tree");
        give.send(tree).expect("the thread waits for the tree");
        Some(Self { jobs: Some(jobs), roots, due: None, handle: Some(handle) })
    }

    /// Sends `job` to the thread.
    /// Error cases:
    /// - the thread has stopped
    fn send(&mut self, job: Job) -> anyhow::Result<()> {
        let sent = self.jobs.as_ref().is_some_and(|jobs| jobs.send(job).is_ok());
        if !sent {
            self.stopped()?;
        }
        Ok(())
    }

    /// Waits for the root due, if any, and checks it against the log's.
    /// Error cases:
    /// - it differs from the log's, or the thread has stopped
    fn check_due(&mut self) -> anyhow::Result<()> {
        let Some((seq, logged)) = self.due.take() else { return Ok(()) };
        match self.roots.recv() {
            Ok(root) => check(seq, &logged, root),
            Err(_) => self.stopped(),
        }
    }

    /// Ends the thread and returns its tree, raising its panic if it panicked.
    fn join(&mut self) -> StateTree {
        self.jobs = None;
        let handle = self.handle.take().expect("a thread is joined once");
        handle.join().unwrap_or_else(|panicked| panic::resume_unwind(panicked))
    }

    /// The thread's end, which only its panic brings about while the replay holds it: raises the panic.
    /// Error cases:
    /// - the thread ended without one
    fn stopped(&mut self) -> anyhow::Result<()> {
        let _ = self.join();
        bail!("the replay's root thread stopped")
    }
}

impl Replay {
    /// Has this replay's state roots computed on a thread of their own from the first checkpoint at which
    /// its engine holds a tree (`Roots`). The caller settles its read (`settled`) before it finishes it.
    pub(in crate::sequencer) fn check_roots_ahead(&mut self) {
        if matches!(self.roots, Roots::Inline) {
            self.roots = Roots::Ahead(None);
        }
    }

    /// Has this replay check the seals of the log at `path` as `seals` says, ahead of it on workers or at
    /// the end of each run (`check_seals_ahead`, `check_run_ends`), and compute its roots on a thread of
    /// their own (`check_roots_ahead`), as a whole replay does. The caller settles its read (`settled`)
    /// before it finishes it.
    pub(in crate::sequencer) fn check_ahead(&mut self, path: &std::path::Path, seals: SealChecks) {
        match seals {
            SealChecks::Every(workers) => self.check_seals_ahead(path, workers),
            SealChecks::RunEnds => self.check_run_ends(),
        }
        self.check_roots_ahead();
    }

    /// Has this replay check every later seal's form, and in full only the last seal of each run of one
    /// signer's seals (`Origin::check_run_ends`), as `engine replay` does. The caller settles its read
    /// (`settled`) before it finishes it, which checks the last run.
    pub(in crate::sequencer) fn check_run_ends(&mut self) {
        self.origin.check_run_ends();
    }

    /// Applies the log's next entry (`apply_entry`), reporting, for an entry that fails, an earlier fault
    /// first (`earlier_fault`).
    /// Error cases:
    /// - as `apply_entry`
    pub(in crate::sequencer) fn apply(&mut self, entry: &LogEntry) -> anyhow::Result<()> {
        self.apply_entry(entry).map_err(|failed| self.earlier_fault().unwrap_or(failed))
    }

    /// What `read`, this replay's read of a log, gives, with its roots and seals settled: an error the
    /// read gives, from an entry the replay or the caller refused or from the read itself, after any
    /// earlier fault (`earlier_fault`); and once the read succeeds, the last root checked and the tree
    /// taken back into the engine, so `finish` hands on an engine that computes its own roots, and then
    /// the last run of seals checked. A replay that checks roots ahead or run ends settles its read with it.
    /// Error cases:
    /// - the read fails, or a root still due differs from the log's, or the root thread has stopped
    /// - the last run's seal does not verify (`Origin::end_run`)
    pub(in crate::sequencer) fn settled<T>(&mut self, read: anyhow::Result<T>) -> anyhow::Result<T> {
        match read {
            Err(failed) => Err(self.earlier_fault().unwrap_or(failed)),
            Ok(read) => {
                self.roots.take_back(&mut self.engine)?;
                self.origin.end_run()?;
                Ok(read)
            }
        }
    }

    /// A fault before the entry the replay is at, which it reports ahead of that entry's: a root still due
    /// that differs from the log's, at the last checkpoint; else the open run's seal that does not verify,
    /// over the commands since that checkpoint, the entry's own command among them once its seal is taken.
    fn earlier_fault(&mut self) -> Option<anyhow::Error> {
        self.roots.check_due().err().or_else(|| self.origin.end_run().err())
    }
}

/// Checks `root`, the replay's root at the checkpoint at `seq`, against `logged`, the log's.
/// Error cases:
/// - they differ
fn check(seq: u64, logged: &str, root: B256) -> anyhow::Result<()> {
    let root = format!("0x{}", hex::encode(root));
    if root != logged {
        bail!("state root mismatch at seq {seq}: log says {logged}, replay gives {root}");
    }
    Ok(())
}

/// The thread: applies each job to `tree` in order, answering each commit with its root, until the
/// replay closes `jobs`, and returns the tree.
fn run(mut tree: StateTree, jobs: Receiver<Job>, roots: Sender<B256>) -> StateTree {
    for job in jobs {
        match job {
            Job::Commit(seq, inputs) => {
                let (root, _) = inputs.apply(&mut tree);
                tree.release(seq, root);
                tree.collect(usize::MAX);
                // A replay that stopped waiting has stopped; it closes `jobs` next.
                let _ = roots.send(root);
            }
            Job::Event(TreeEvent::Reset) => tree.reset(),
            Job::Event(TreeEvent::Discard) => tree = StateTree::default(),
        }
    }
    tree
}

#[cfg(test)]
mod tests;
