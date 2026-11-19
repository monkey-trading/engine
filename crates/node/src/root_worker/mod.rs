//! The root worker (#925): a thread that owns the state tree once the sequencer detaches it from its
//! engine, so that applying the leaf writes a mass change makes, such as the daily rollover's, never
//! holds a price. The sequencer copies the writes out of its engine in pieces as it goes
//! (`Engine::take_root_delta`) and sends them here in order; at a checkpoint it sends the rest
//! (`Engine::commit_inputs`) and waits for the root, which it bounds by deferring a paced checkpoint
//! until little is left to write. It releases each checkpoint's version once the log holds the
//! checkpoint durably, and every proof and claim the API serves is read here from the newest released
//! version, in steps a waiting commit goes ahead of; a payout's first proof is read from its
//! checkpoint's version, whose signature waits for the vault to accept that checkpoint. The tree's results are a function of the messages alone,
//! in the order the sequencer sent them, so the roots are those an inline engine computes.

mod claims;
pub use claims::ClaimAnswer;
pub(crate) mod protocol;
mod state;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError, channel, sync_channel};

use anyhow::{Context, anyhow};
use engine_core::eip712::B256;
use engine_core::merkle::tree::StateTree;
use engine_core::merkle::{
    AccountProof, CommitInputs, DistributionProof, EntitlementProof, ExitProof, RootDelta, RootWork,
    WithdrawalApprovalProof,
};
use engine_core::types::AccountId;
use tokio::sync::{Notify, oneshot};
use tracing::{debug, error};

use crate::checkpoints::LatestCheckpoint;
use crate::root_worker::protocol::Progress;
use crate::root_worker::state::WorkerState;

/// Reads forwarded and not yet answered past which the sequencer turns more away, answering them
/// unavailable, so a burst of reads holds a bounded queue on the worker, however long the worker's
/// tree work keeps it from them.
pub(crate) const MAX_READS: usize = 1024;

/// A payout's first proof, `None` where the worker no longer held the checkpoint's version.
pub(crate) type Proved = ((AccountId, u64), Option<WithdrawalApprovalProof>);

/// A message to the worker.
pub(crate) enum Message {
    Work(Work),
    Read(Read),
}

/// The sequencer's messages, which the worker handles strictly in the order sent.
pub(crate) enum Work {
    /// The tree the sequencer's engine held, replacing any the worker has.
    Install(Box<StateTree>),
    /// Leaf writes, and how many units they count for the sequencer's bound.
    Advance(RootDelta, u64),
    /// Back to the newest committed version, after a protocol version change.
    Reset,
    /// No version any more, after an activation that changes leaves no command touched.
    Discard,
    /// The rest of a root's writes, and where to answer with the root.
    Commit(CommitInputs, SyncSender<(B256, RootWork)>),
    /// The checkpoint at this seq with this root is durable in the log.
    Release(u64, B256),
    /// The first proofs of these approvals, against the version of the checkpoint at this seq.
    Prove(u64, Vec<(AccountId, u64)>),
    /// The vault's latest checkpoint, which claims are answered at.
    Accept(LatestCheckpoint),
    /// Panics on the worker's thread, for a test of the sequencer stopping with it.
    #[cfg(test)]
    Panic,
    /// Holds the worker's thread until the sender sends or drops, for a test of what waits on it.
    #[cfg(test)]
    Hold(Receiver<()>),
}

/// A proof read against the newest released version, which the sequencer forwards from the API.
pub(crate) enum Read {
    Account(AccountId, oneshot::Sender<Option<AccountProof>>),
    Exit(AccountId, oneshot::Sender<Option<ExitProof>>),
    Entitlement(AccountId, oneshot::Sender<Option<ClaimAnswer<EntitlementProof>>>),
    Distribution(AccountId, oneshot::Sender<Option<ClaimAnswer<DistributionProof>>>),
}

impl Read {
    /// Whether the API has stopped waiting for the answer.
    pub(crate) fn is_closed(&self) -> bool {
        match self {
            Self::Account(_, reply) => reply.is_closed(),
            Self::Exit(_, reply) => reply.is_closed(),
            Self::Entitlement(_, reply) => reply.is_closed(),
            Self::Distribution(_, reply) => reply.is_closed(),
        }
    }
}

/// What the worker reports to the sequencer.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The first proofs a `Prove` asked for.
    Proved { seq: u64, proofs: Vec<Proved> },
    /// The worker's thread has ended, and why: the sequencer stops with it.
    Stopped(String),
}

/// What the sequencer and the worker share.
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) progress: Progress,
    /// Reads forwarded and not yet answered, or dropped once the API stopped waiting.
    pub(crate) reads: AtomicUsize,
    /// Notified when the worker reaches an armed wake-up, reports an outcome, or stops.
    pub(crate) woken: Notify,
}

/// The sequencer's end of the worker.
pub(crate) struct RootWorker {
    tx: Sender<Message>,
    outcomes: Receiver<Outcome>,
    /// Reports taken while waiting for another (`prove_now`), handed out first by `outcomes`.
    stashed: Vec<Outcome>,
    shared: Arc<Shared>,
    /// Units of leaf writes sent.
    sent: u64,
}

/// Reports a panic of the worker's thread to the sequencer, which stops with it. The thread's other
/// ends, every sender gone or the sequencer stopped, leave no one to tell.
struct StopGuard {
    outcomes: Sender<Outcome>,
    shared: Arc<Shared>,
}

impl RootWorker {
    /// Starts the worker's thread, holding no tree until the sequencer installs one.
    /// Error cases:
    /// - the thread cannot start
    pub(crate) fn start() -> anyhow::Result<Self> {
        let (tx, rx) = channel();
        let (outcomes_tx, outcomes) = channel();
        let shared = Arc::new(Shared::default());
        let worker_shared = shared.clone();
        std::thread::Builder::new()
            .name("state-root".into())
            .spawn(move || run(WorkerState::new(StateTree::default()), &rx, &worker_shared, outcomes_tx))
            .context("starting the state root worker")?;
        Ok(Self { tx, outcomes, stashed: Vec::new(), shared, sent: 0 })
    }

    /// Sends `work`, which the worker handles after everything sent before it.
    /// Error cases:
    /// - the worker has stopped
    fn send(&self, work: Work) -> anyhow::Result<()> {
        self.tx.send(Message::Work(work)).map_err(|_| anyhow!("the state root worker has stopped"))
    }

    /// Hands the worker the tree the engine held.
    /// Error cases:
    /// - the worker has stopped
    pub(crate) fn install(&self, tree: StateTree) -> anyhow::Result<()> {
        self.send(Work::Install(Box::new(tree)))
    }

    /// Sends leaf writes for the working version.
    /// Error cases:
    /// - the worker has stopped
    pub(crate) fn advance(&mut self, delta: RootDelta) -> anyhow::Result<()> {
        let units = delta.len() as u64;
        self.send(Work::Advance(delta, units))?;
        self.sent += units;
        Ok(())
    }

    /// Sends a root's last writes and waits for the root. Reads and background steps wait meanwhile,
    /// and the worker takes no other step but those of the work sent before the commit.
    /// Error cases:
    /// - the worker has stopped, or stops before answering
    pub(crate) fn commit(&mut self, inputs: CommitInputs) -> anyhow::Result<(B256, RootWork)> {
        let (reply, answer) = sync_channel(1);
        self.shared.progress.set_commit_waiting(true);
        let answered = self.send(Work::Commit(inputs, reply)).and_then(|()| {
            answer.recv().map_err(|_| anyhow!("the state root worker stopped before answering a commit"))
        });
        self.shared.progress.set_commit_waiting(false);
        answered
    }

    /// Releases the version of a checkpoint the log now holds durably.
    /// Error cases:
    /// - the worker has stopped
    pub(crate) fn release(&self, seq: u64, root: B256) -> anyhow::Result<()> {
        self.send(Work::Release(seq, root))
    }

    /// Discards the working version's writes since the last commit.
    /// Error cases:
    /// - the worker has stopped
    pub(crate) fn reset(&self) -> anyhow::Result<()> {
        self.send(Work::Reset)
    }

    /// Discards every version.
    /// Error cases:
    /// - the worker has stopped
    pub(crate) fn discard(&self) -> anyhow::Result<()> {
        self.send(Work::Discard)
    }

    /// Asks for the first proofs of `approvals` at the checkpoint at `seq`, answered as an outcome.
    /// Error cases:
    /// - the worker has stopped
    pub(crate) fn prove(&self, seq: u64, approvals: Vec<(AccountId, u64)>) -> anyhow::Result<()> {
        self.send(Work::Prove(seq, approvals))
    }

    /// Forwards a proof read, which the worker answers on the read's own channel, or, with
    /// `MAX_READS` outstanding, drops it, which answers the API unavailable.
    /// Error cases:
    /// - the worker has stopped
    pub(crate) fn read(&self, read: Read) -> anyhow::Result<()> {
        if read.is_closed() {
            return Ok(());
        }
        // Only the sequencer adds, so no other read can pass the bound between the check and the add.
        if self.shared.reads.load(Ordering::Acquire) >= MAX_READS {
            debug!("a proof read turned away: {MAX_READS} reads wait on the state root worker");
            return Ok(());
        }
        self.shared.reads.fetch_add(1, Ordering::AcqRel);
        self.tx.send(Message::Read(read)).map_err(|_| anyhow!("the state root worker has stopped"))
    }

    /// The worker's next report, waiting for it.
    /// Error cases:
    /// - the worker has stopped
    pub(crate) fn next_outcome(&mut self) -> anyhow::Result<Outcome> {
        if !self.stashed.is_empty() {
            return Ok(self.stashed.remove(0));
        }
        match self.outcomes.recv() {
            Ok(Outcome::Stopped(reason)) => anyhow::bail!("the state root worker stopped: {reason}"),
            Ok(outcome) => Ok(outcome),
            Err(_) => anyhow::bail!("the state root worker stopped"),
        }
    }

    /// `prove`, waiting for the answer, for mock custody, which signs a payout right after the
    /// checkpoint that covers it.
    /// Error cases:
    /// - the worker has stopped
    pub(crate) fn prove_now(&mut self, seq: u64, approvals: Vec<(AccountId, u64)>) -> anyhow::Result<Vec<Proved>> {
        self.send(Work::Prove(seq, approvals))?;
        loop {
            match self.outcomes.recv() {
                Ok(Outcome::Proved { seq: proved, proofs }) if proved == seq => return Ok(proofs),
                Ok(Outcome::Stopped(reason)) => anyhow::bail!("the state root worker stopped: {reason}"),
                Ok(other) => self.stashed.push(other),
                Err(_) => anyhow::bail!("the state root worker stopped"),
            }
        }
    }

    /// Passes on the vault's latest checkpoint, for claims.
    /// Error cases:
    /// - the worker has stopped
    pub(crate) fn accept(&self, latest: LatestCheckpoint) -> anyhow::Result<()> {
        self.send(Work::Accept(latest))
    }

    /// What panics the worker's thread once called, for a test that has handed the sequencer to `run`.
    #[cfg(test)]
    pub(crate) fn panic_switch(&self) -> impl FnOnce() + Send + 'static {
        let tx = self.tx.clone();
        move || _ = tx.send(Message::Work(Work::Panic))
    }

    /// Holds the worker's thread once it reaches this, with everything sent after it waiting, until the
    /// returned sender sends or drops, for a test of what waits on the worker.
    /// Error cases:
    /// - the worker has stopped
    #[cfg(test)]
    pub(crate) fn hold(&self) -> anyhow::Result<Sender<()>> {
        let (release, held) = channel();
        self.send(Work::Hold(held))?;
        Ok(release)
    }

    /// Units of leaf writes sent that the worker has yet to finish.
    pub(crate) fn pending(&self) -> u64 {
        self.sent.saturating_sub(self.shared.progress.finished())
    }

    /// Arms a wake-up for when no more than `pending` units wait, and returns whether that already
    /// holds, in which case nothing stays armed.
    pub(crate) fn wake_at_pending(&self, pending: u64) -> bool {
        self.shared.progress.wake_when(self.sent.saturating_sub(pending))
    }

    /// The worker's reports so far.
    /// Error cases:
    /// - the worker has stopped: the sequencer must stop with it
    pub(crate) fn outcomes(&mut self) -> anyhow::Result<Vec<Outcome>> {
        let mut outcomes = std::mem::take(&mut self.stashed);
        loop {
            match self.outcomes.try_recv() {
                Ok(Outcome::Stopped(reason)) => anyhow::bail!("the state root worker stopped: {reason}"),
                Ok(outcome) => outcomes.push(outcome),
                Err(TryRecvError::Empty) => return Ok(outcomes),
                Err(TryRecvError::Disconnected) => anyhow::bail!("the state root worker stopped"),
            }
        }
    }

    /// What the worker shares with the sequencer, for a wait that also holds the sequencer.
    pub(crate) fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        error!("the state root worker stopped: it panicked");
        let _ = self.outcomes.send(Outcome::Stopped("it panicked".into()));
        self.shared.woken.notify_one();
    }
}

/// The worker's loop: takes every message waiting into its lane, takes one step, reports what it
/// did, and waits for a message only with every lane empty. It ends once every sender is gone and
/// nothing is left to do.
fn run(mut state: WorkerState, rx: &Receiver<Message>, shared: &Arc<Shared>, outcomes: Sender<Outcome>) {
    let guard = StopGuard { outcomes, shared: shared.clone() };
    loop {
        loop {
            match rx.try_recv() {
                Ok(message) => state.take(message),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) if state.idle() => return,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        let Some(finished) = state.step(&shared.progress) else {
            match rx.recv() {
                Ok(message) => state.take(message),
                Err(_) => return,
            }
            continue;
        };
        let woken = finished > 0 && shared.progress.finish(finished);
        let answered = state.take_answered();
        if answered > 0 {
            shared.reads.fetch_sub(answered, Ordering::AcqRel);
        }
        let reports = state.take_outbox();
        let reported = !reports.is_empty();
        for report in reports {
            if guard.outcomes.send(report).is_err() {
                // The sequencer has stopped, so no one is left to tell.
                return;
            }
        }
        if woken || reported {
            shared.woken.notify_one();
        }
    }
}

#[cfg(test)]
mod tests;
