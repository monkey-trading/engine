//! The sequencer's side of the root worker (#925). Once its engine's tree is detached — when `run`
//! starts on a log at `V9` or later whose engine holds a root, or at the first root under `V9` — the
//! sequencer keeps no tree: it copies the leaf writes its commands make out of the engine in pieces
//! of about a millisecond, between batches and while it has no request, and sends them to the worker
//! in order. A checkpoint's root is the worker's commit of the rest, which the sequencer waits for.
//! A paced checkpoint goes ahead only once that wait is within `ROOT_BOUND`, and is taken anyway, in
//! full, under the Warning `root_backlog_held` after `ROOT_DEFER_MAX`; an unpaced one never waits.
//! Each checkpoint's version is released once the batch that holds the checkpoint is durable, and the
//! API's proofs and claims are read on the worker from the newest released version. A payout's first
//! proof is read from the version of the checkpoint that covers it, released or not, since its
//! signature waits for the vault to accept that checkpoint.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use engine_core::eip712::B256;
use engine_core::merkle::TreeEvent;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use prometheus_client::metrics::counter::Counter;
use tokio::time::Instant as TokioInstant;
use tracing::{debug, info, warn};

use crate::root_worker::{Outcome, Read, RootWorker, Shared};
use crate::sequencer::{Request, Sequencer};

/// The longest the sequencer copies leaf writes out of its engine at a time.
const PIECE_TIME: Duration = Duration::from_millis(1);
/// Entries copied between looks at the time.
const PIECE_ENTRIES: usize = 128;
/// The root work a paced checkpoint's commit may wait for, in leaf writes on the worker:
/// `launch_book_benchmark`'s `root_commit` cases put a wait at this bound at about 25 ms on CI.
const ROOT_BOUND: u64 = 4_000;
/// What a commit waits for beside its leaf writes, in leaf writes: hashing the branches nearly every
/// write shares, which only a commit hashes, and the commit's bookkeeping.
const ROOT_RESERVE: u64 = 1_000;
/// What an entry still in the engine's record costs a commit, in leaf writes: it is copied out on the
/// sequencer and written on the worker within the wait.
const TOUCHED_WEIGHT: u64 = 2;
/// The engine's record, in entries, above which the sequencer copies writes out between checkpoints is
/// the bound over this, half of what the bound allows it.
const EXTRACT_SHARE: u64 = 2 * TOUCHED_WEIGHT;
/// Leaf writes waiting on the worker above which the sequencer sends no more is the bound times this,
/// so the engine's record holds the rest, and a key changed again is copied once.
const QUEUE_SHARE: u64 = 8;
/// How long a paced checkpoint waits for its root work before it is taken in full.
pub(super) const ROOT_DEFER_MAX: Duration = Duration::from_secs(30);

/// The sequencer's state for the root worker.
pub(super) struct Roots {
    pub(super) worker: RootWorker,
    /// Whether `run` has started: the tree is detached from then on, and before it the engine
    /// computes its roots inline, as recovery and setup do.
    running: bool,
    /// Whether the worker holds the tree: the engine computes no root of its own.
    detached: bool,
    /// Payout proofs asked of the worker and not yet answered, by the checkpoint they are asked at,
    /// with that checkpoint's root.
    proving: BTreeMap<u64, (usize, B256)>,
    /// Since when a paced checkpoint has waited for root work.
    deferred: Option<TokioInstant>,
    /// Attempts that deferred for root work, so a retry that does not defer again ends the wait.
    deferrals: u64,
    /// Whether `root_backlog_held` is open.
    held: bool,
    /// The versions of checkpoints written in the batch being taken, released once it is durable.
    pub(super) releases: Vec<(u64, B256)>,
    /// `ROOT_BOUND` and `ROOT_DEFER_MAX`, which tests set lower.
    bound: u64,
    defer_max: Duration,
}

impl Roots {
    /// Starts the worker, which holds no tree until the sequencer detaches its engine's, counting the
    /// reads it turns away on `reads_refused`.
    /// Error cases:
    /// - the worker's thread cannot start
    pub(super) fn start(reads_refused: Counter) -> anyhow::Result<Self> {
        Ok(Self {
            worker: RootWorker::start(reads_refused)?,
            running: false,
            detached: false,
            proving: BTreeMap::new(),
            deferred: None,
            deferrals: 0,
            held: false,
            releases: Vec::new(),
            bound: ROOT_BOUND,
            defer_max: ROOT_DEFER_MAX,
        })
    }

    /// What the worker shares with the sequencer's wait for a request.
    pub(super) fn shared(&self) -> Arc<Shared> {
        self.worker.shared()
    }

    /// Whether the worker holds the tree.
    pub(super) fn detached(&self) -> bool {
        self.detached
    }

    /// When a deferred checkpoint is taken whatever its root work.
    pub(super) fn deadline(&self) -> Option<TokioInstant> {
        self.deferred.map(|since| since + self.defer_max)
    }

    /// The checkpoints payout proofs are asked at and not yet answered, with their roots.
    pub(super) fn proving(&self) -> impl Iterator<Item = (u64, B256)> + '_ {
        self.proving.iter().map(|(seq, (_, root))| (*seq, *root))
    }

    /// A lower bound and deferral limit, for tests that reach them with a small book.
    #[cfg(test)]
    pub(super) fn set_bounds(&mut self, bound: u64, defer_max: Duration) {
        (self.bound, self.defer_max) = (bound, defer_max);
    }
}

impl Sequencer {
    /// Starts computing roots on the worker as `run` starts: the engine's tree is handed over now,
    /// or at the first root under `V9`.
    /// Error cases:
    /// - the worker has stopped
    pub(super) fn start_root_worker(&mut self) -> anyhow::Result<()> {
        self.roots.running = true;
        self.detach_tree()
    }

    /// Hands the engine's tree to the worker when the log is at `V9` or later and the engine holds
    /// one, as at a start from a replayed log. Its version is released at once when the log holds a
    /// checkpoint at its seq with its root, and otherwise waits for the next checkpoint's.
    /// Error cases:
    /// - the worker has stopped
    fn detach_tree(&mut self) -> anyhow::Result<()> {
        if !self.roots.running || self.roots.detached || self.engine.protocol_version() < ProtocolVersion::V9 {
            return Ok(());
        }
        let Some(tree) = self.engine.detach_tree() else { return Ok(()) };
        let version = tree.newest().map(|version| (version.seq(), version.root()));
        self.roots.worker.install(tree)?;
        self.roots.detached = true;
        let logged = self.house.latest_checkpoint().map(|cp| (cp.seq, cp.root.clone()));
        if let Some((seq, root)) = version
            && logged == Some((seq, format!("0x{}", hex::encode(root))))
        {
            self.roots.worker.release(seq, root)?;
        }
        info!(version = ?version.map(|(seq, _)| seq), "state tree handed to the root worker");
        Ok(())
    }

    /// Whether a paced checkpoint at `seq` waits for root work: while the tree is detached and a
    /// commit now would wait past `ROOT_BOUND`, until `ROOT_DEFER_MAX` has passed since it first
    /// waited, after which it is taken in full under `root_backlog_held`. An unpaced one never waits.
    pub(super) fn defers_for_root_work(&mut self, seq: u64, paced: bool) -> bool {
        let wait = self.root_wait();
        if !paced || !self.roots.detached || wait <= self.roots.bound {
            return false;
        }
        let now = TokioInstant::now();
        let since = *self.roots.deferred.get_or_insert(now);
        if now.duration_since(since) < self.roots.defer_max {
            self.roots.deferrals += 1;
            debug!(seq, wait, "a paced checkpoint waits for root work");
            return true;
        }
        if !self.roots.held {
            self.roots.held = true;
            warn!(
                alert = "root_backlog_held",
                "a paced checkpoint waited {}s for root work; taking it in full at seq {seq} ({wait} leaf writes)",
                self.roots.defer_max.as_secs()
            );
        }
        false
    }

    /// The root of a checkpoint at `seq`, the last command: the engine's own before its tree is
    /// detached, and the worker's commit after, which ends any deferral.
    /// Error cases:
    /// - the worker has stopped
    pub(super) fn checkpoint_root(&mut self, seq: u64) -> anyhow::Result<B256> {
        if !self.roots.detached {
            let root = B256::from(self.engine.state_root());
            // The first root under `V9` in `run` hands the tree over.
            self.detach_tree()?;
            return Ok(root);
        }
        let (started, wait) = (Instant::now(), self.root_wait());
        let inputs = self.engine.commit_inputs();
        let (root, work) = self.roots.worker.commit(inputs)?;
        self.metrics.state_root.commit_seconds.observe(started.elapsed().as_secs_f64());
        debug!(seq, wait, leaves = work.leaves, nodes = work.nodes, "state root committed");
        self.roots.deferred = None;
        if std::mem::take(&mut self.roots.held) {
            info!(alert_resolved = "root_backlog_held", "a paced checkpoint's root work is done");
        }
        Ok(root)
    }

    /// What a commit now would wait for, in leaf writes on the worker.
    fn root_wait(&self) -> u64 {
        self.roots.worker.pending() + self.engine.root_backlog() as u64 * TOUCHED_WEIGHT + ROOT_RESERVE
    }

    /// Tells the worker what a protocol version change the last command made asks of it, before any
    /// later leaf write.
    /// Error cases:
    /// - the worker has stopped
    pub(super) fn send_tree_event(&mut self) -> anyhow::Result<()> {
        match self.engine.take_tree_event() {
            Some(TreeEvent::Reset) => self.roots.worker.reset(),
            Some(TreeEvent::Discard) => self.roots.worker.discard(),
            None => Ok(()),
        }
    }

    /// The root work at a batch's end: the worker's reports, then a piece of leaf writes to copy out.
    /// Error cases:
    /// - the worker has stopped
    pub(super) fn root_batch_end(&mut self) -> anyhow::Result<()> {
        self.take_root_outcomes()?;
        self.copy_root_work()
    }

    /// Copies leaf writes out of the engine to the worker for up to `PIECE_TIME`, while the record
    /// is above half what the bound allows it and the worker has room.
    /// Error cases:
    /// - the worker has stopped
    pub(super) fn copy_root_work(&mut self) -> anyhow::Result<()> {
        if !self.roots.detached {
            return Ok(());
        }
        let started = Instant::now();
        let mut copied = false;
        while self.root_copy_due() && started.elapsed() < PIECE_TIME {
            let delta = self.engine.take_root_delta(PIECE_ENTRIES);
            self.roots.worker.advance(delta)?;
            copied = true;
        }
        if copied {
            self.metrics.state_root.piece_seconds.observe(started.elapsed().as_secs_f64());
        }
        self.metrics.state_root.backlog.set(i64::try_from(self.root_wait()).unwrap_or(i64::MAX));
        Ok(())
    }

    /// Whether there are leaf writes to copy out and room on the worker for them.
    fn root_copy_due(&self) -> bool {
        let (bound, parts) = (self.roots.bound, self.engine.root_part_backlog() as u64);
        parts > bound / EXTRACT_SHARE && self.roots.worker.pending() < QUEUE_SHARE * bound
    }

    /// Whether the sequencer has root work to do now rather than wait for a request: writes to copy
    /// out, or a deferred checkpoint whose wait is within the bound or whose time is up. Called before
    /// each wait, it otherwise arms a wake-up, while a checkpoint is deferred, for the worker's progress
    /// that makes work due: the wait within the bound, or, while the engine's record alone is over it,
    /// room on the worker for more of the record.
    pub(super) fn root_work_due(&self) -> bool {
        if !self.roots.detached {
            return false;
        }
        if self.root_copy_due() {
            return true;
        }
        let Some(since) = self.roots.deferred else { return false };
        if TokioInstant::now().duration_since(since) >= self.roots.defer_max {
            return true;
        }
        let (bound, record) = (self.roots.bound, self.engine.root_backlog() as u64);
        let parts = self.engine.root_part_backlog() as u64;
        let pending = match bound.checked_sub(record * TOUCHED_WEIGHT + ROOT_RESERVE) {
            Some(pending) => pending,
            None if parts > bound / EXTRACT_SHARE => (QUEUE_SHARE * bound).saturating_sub(1),
            // No progress of the worker's brings this wait within the bound: its deadline ends it.
            None => return false,
        };
        // True when already reached, in which case nothing stays armed.
        self.roots.worker.wake_at_pending(pending)
    }

    /// Handles a wake-up from the worker, the deferral's deadline, or idle time: takes the worker's
    /// reports, copies leaf writes out, and retries a deferred checkpoint.
    /// Error cases:
    /// - the worker has stopped, or the checkpoint cannot be written
    pub(super) fn root_wake(&mut self) -> anyhow::Result<()> {
        self.take_root_outcomes()?;
        self.copy_root_work()?;
        self.retry_deferred_checkpoint()
    }

    /// At a batch's end, before it is staged, writes a checkpoint that waited: for a member to confirm
    /// holding its seq, or deferred for root work, so requests that arrive faster than the sequencer
    /// waits for one, which leave it no wake-up to take, keep neither waiting.
    /// Error cases:
    /// - a checkpoint cannot be written
    pub(super) fn retry_waiting_checkpoints(&mut self) -> anyhow::Result<()> {
        self.release_waiting_checkpoint()?;
        self.retry_deferred_checkpoint()
    }

    /// Takes a deferred checkpoint whose wait is now within the bound or whose time is up.
    /// Error cases:
    /// - the checkpoint cannot be written
    fn retry_deferred_checkpoint(&mut self) -> anyhow::Result<()> {
        let Some(since) = self.roots.deferred else { return Ok(()) };
        let deadline = TokioInstant::now().duration_since(since) >= self.roots.defer_max;
        if self.root_wait() <= self.roots.bound || deadline {
            let deferrals = self.roots.deferrals;
            self.paced_checkpoint()?;
            if self.roots.deferrals == deferrals {
                // Written, or held back by the gate, the liveness rule or a full posting queue, or
                // covered by another checkpoint: their own schedules take the next one.
                self.roots.deferred = None;
            }
        }
        Ok(())
    }

    /// Takes the worker's reports: payout proofs arriving for approvals a checkpoint covers.
    /// Error cases:
    /// - the worker has stopped
    pub(super) fn take_root_outcomes(&mut self) -> anyhow::Result<()> {
        for outcome in self.roots.worker.outcomes()? {
            self.take_root_outcome(outcome)?;
        }
        Ok(())
    }

    /// Asks the worker for the first proofs of `approvals` at the checkpoint at `seq`, of root `root`.
    /// Error cases:
    /// - the worker has stopped
    pub(super) fn prove_payouts(
        &mut self,
        seq: u64,
        root: B256,
        approvals: Vec<(AccountId, u64)>,
    ) -> anyhow::Result<()> {
        self.roots.worker.prove(seq, approvals)?;
        self.roots.proving.entry(seq).or_insert((0, root)).0 += 1;
        Ok(())
    }

    /// Waits for the payout proofs asked at checkpoints the vault has posted, up to `posted`, which a
    /// release of payouts may sign with now; proofs at a later checkpoint arrive as outcomes, long before
    /// that checkpoint is final. The worker asked for them when their checkpoint was written, or, for
    /// approvals a recovery window has just loaded, at a checkpoint already covering them, so the wait
    /// is that window's proofs at most.
    /// Error cases:
    /// - the worker has stopped
    pub(super) fn settle_payout_proofs(&mut self, posted: Option<u64>) -> anyhow::Result<()> {
        self.take_root_outcomes()?;
        let due = |proving: &BTreeMap<u64, (usize, B256)>| {
            posted.is_some_and(|posted| proving.range(..=posted).next().is_some())
        };
        while due(&self.roots.proving) {
            let outcome = self.roots.worker.next_outcome()?;
            self.take_root_outcome(outcome)?;
        }
        Ok(())
    }

    /// Takes one report of the worker's.
    /// Error cases:
    /// - the worker has stopped
    fn take_root_outcome(&mut self, outcome: Outcome) -> anyhow::Result<()> {
        match outcome {
            Outcome::Proved { seq, proofs } => {
                if let Some((asked, _)) = self.roots.proving.get_mut(&seq) {
                    *asked -= 1;
                    if *asked == 0 {
                        self.roots.proving.remove(&seq);
                    }
                }
                self.take_payout_proofs(seq, proofs);
                Ok(())
            }
            Outcome::Stopped(reason) => anyhow::bail!("the state root worker stopped: {reason}"),
        }
    }

    /// Forwards a proof or claim read from the API to the worker, which answers it on its reply.
    /// Error cases:
    /// - the worker has stopped
    pub(super) fn forward_read(&mut self, request: Request) -> anyhow::Result<()> {
        let read = match request {
            Request::AccountProof { account, reply } => Read::Account(account, reply),
            Request::ExitProof { account, reply } => Read::Exit(account, reply),
            Request::EntitlementClaim { account, reply } => Read::Entitlement(account, reply),
            Request::DistributionClaim { account, reply } => Read::Distribution(account, reply),
            _ => unreachable!("only proof and claim reads are forwarded"),
        };
        self.roots.worker.read(read).context("forwarding a proof read")
    }
}

#[cfg(test)]
mod tests;
