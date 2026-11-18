//! The root worker's logic, apart from its thread: what it holds, and each step it takes. No step
//! waits on anything, and each takes about a millisecond at most, but a commit's, whose size the
//! sequencer bounds before it sends one.

use std::collections::VecDeque;

use engine_core::merkle::tree::{ExitItem, StateTree};
use engine_core::merkle::{AccountProof, ExitProof};
use engine_core::types::AccountId;
use tracing::debug;

use crate::root_worker::claims::Claims;
use crate::root_worker::protocol::{Lane, Progress, Word};
use crate::root_worker::{Message, Outcome, Proved, Read, Work};

/// Nodes freed in one step.
const FREE_STEP: usize = 20_000;
/// Proofs taken in one step of a claim capture, a payout's proofs, or an exit read's items and approvals.
const PROOF_STEP: usize = 32;

/// What the worker holds: the tree, and each lane's queue.
pub(super) struct WorkerState {
    tree: StateTree,
    work: VecDeque<Work>,
    reads: VecDeque<Reading>,
    background: VecDeque<Background>,
    claims: Claims,
    last: Lane,
    /// What the steps taken since the thread last looked have to report to the sequencer.
    outbox: Vec<Outcome>,
    /// Reads answered, or dropped unanswered, since the thread last looked.
    answered: usize,
}

/// A read in progress.
enum Reading {
    Read(Read),
    /// An exit proof whose items, then approvals, are read a step at a time, against the version it
    /// pinned.
    Exit(Box<Exiting>),
}

impl Reading {
    fn is_closed(&self) -> bool {
        match self {
            Self::Read(read) => read.is_closed(),
            Self::Exit(exiting) => exiting.reply.is_closed(),
        }
    }
}

/// An exit proof under way: its head and the items proved so far, where the listing of its items has
/// reached, and the approvals still to prove.
struct Exiting {
    version: u64,
    account: AccountId,
    proof: ExitProof,
    /// The last item proved, from which the listing goes on.
    last: Option<ExitItem>,
    /// Whether the listing has ended.
    listed: bool,
    /// The next approval nonce to prove, up to the account leaf's `withdrawal_nonce`.
    next: u64,
    reply: tokio::sync::oneshot::Sender<Option<ExitProof>>,
}

/// What no root waits for.
enum Background {
    /// Nodes no version holds any more.
    Free,
    /// The watched accounts' claim proofs at a version just committed, which they pin.
    Capture { version: u64, accounts: Vec<AccountId> },
    /// A payout's first proofs at the version of the checkpoint at `seq`, which they pin.
    Prove { version: u64, seq: u64, approvals: VecDeque<(AccountId, u64)>, proofs: Vec<Proved> },
}

impl WorkerState {
    /// A worker holding `tree`, with every lane empty.
    pub(super) fn new(tree: StateTree) -> Self {
        Self {
            tree,
            work: VecDeque::new(),
            reads: VecDeque::new(),
            background: VecDeque::new(),
            claims: Claims::default(),
            last: Lane::Background,
            outbox: Vec::new(),
            answered: 0,
        }
    }

    /// Queues a message in its lane. A tree handed over with nothing queued ahead of it, a release whose
    /// version is committed, and the vault's latest checkpoint apply at once, so every read the
    /// sequencer forwards after them sees them: the sequencer sends a release only once the commit it
    /// names has answered, or right after the tree that holds its version.
    pub(super) fn take(&mut self, message: Message) {
        match message {
            Message::Work(Work::Install(tree)) if self.work.is_empty() && self.background.is_empty() => {
                self.cancel();
                self.tree = *tree;
            }
            Message::Work(Work::Release(seq, root)) if self.tree.release(seq, root) => self.queue_free(),
            // Claims read nothing of the tree's, so the vault's checkpoint counts for every claim read
            // forwarded after it.
            Message::Work(Work::Accept(latest)) => self.claims.accept(latest),
            Message::Work(work) => self.work.push_back(work),
            Message::Read(read) => self.reads.push_back(Reading::Read(read)),
        }
    }

    /// Whether every lane is empty.
    pub(super) fn idle(&self) -> bool {
        self.work.is_empty() && self.reads.is_empty() && self.background.is_empty()
    }

    /// Takes the next step `progress` picks, if any lane holds one, and returns the leaf writes it
    /// finished, in the sequencer's units.
    pub(super) fn step<W: Word>(&mut self, progress: &Progress<W>) -> Option<u64> {
        let (work, reads, background) = (!self.work.is_empty(), !self.reads.is_empty(), !self.background.is_empty());
        let lane = progress.next_lane(self.last, work, reads, background)?;
        self.last = lane;
        Some(match lane {
            Lane::Work => self.work_step(progress),
            Lane::Read => {
                self.read_step();
                0
            }
            Lane::Background => {
                self.background_step();
                0
            }
        })
    }

    /// What the steps taken so far have to report, clearing it.
    pub(super) fn take_outbox(&mut self) -> Vec<Outcome> {
        std::mem::take(&mut self.outbox)
    }

    /// Reads answered, or dropped, since the last call.
    pub(super) fn take_answered(&mut self) -> usize {
        std::mem::take(&mut self.answered)
    }

    /// The sequencer's next message, in the order it sent them, returning the leaf writes it finished.
    fn work_step<W: Word>(&mut self, progress: &Progress<W>) -> u64 {
        match self.work.pop_front().expect("next_lane picks a lane that holds a step") {
            Work::Install(tree) => {
                self.cancel();
                self.tree = *tree;
            }
            Work::Advance(delta, units) => {
                delta.apply(&mut self.tree);
                self.queue_free();
                return units;
            }
            Work::Reset => {
                self.tree.reset();
                self.queue_free();
            }
            Work::Discard => {
                self.cancel();
                self.tree = StateTree::default();
            }
            Work::Commit(inputs, reply) => {
                let answer = inputs.apply(&mut self.tree);
                // Cleared before the answer, so the next step takes every lane with nothing more sent;
                // the sequencer sets it again only for its next commit, once it holds this answer.
                progress.set_commit_waiting(false);
                // A sequencer that stopped waiting has stopped: its thread sees the closed channel.
                let _ = reply.send(answer);
                let version = self.tree.newest().expect("committed above").id();
                let accounts = self.claims.watched();
                if !accounts.is_empty() {
                    self.tree.pin(version);
                    self.background.push_back(Background::Capture { version, accounts });
                }
                self.queue_free();
            }
            Work::Release(seq, root) => {
                if !self.tree.release(seq, root) {
                    debug!(seq, %root, "no version to release: the tree was discarded or replaced since");
                }
                self.queue_free();
            }
            Work::Prove(seq, approvals) => {
                let Some(version) = self.tree.at_seq(seq).map(|version| version.id()) else {
                    let proofs = approvals.into_iter().map(|key| (key, None)).collect();
                    self.outbox.push(Outcome::Proved { seq, proofs });
                    return 0;
                };
                self.tree.pin(version);
                let approvals = approvals.into_iter().collect();
                // Ahead of every freeing and claim capture, behind the proofs asked for before it: the
                // sequencer may wait on it before it signs a payout.
                let at = self.background.iter().take_while(|job| matches!(job, Background::Prove { .. })).count();
                self.background.insert(at, Background::Prove { version, seq, approvals, proofs: Vec::new() });
            }
            Work::Accept(_) => unreachable!("`take` applies the vault's checkpoint as it arrives"),
            #[cfg(test)]
            Work::Panic => panic!("a test panicked the state root worker"),
        }
        0
    }

    /// The oldest read's next step, against the newest released version, after dropping the reads at
    /// the front whose API has stopped waiting, with any version an exit read under way pinned.
    fn read_step(&mut self) {
        while let Some(reading) = self.reads.pop_front_if(|reading| reading.is_closed()) {
            if let Reading::Exit(exiting) = reading {
                self.tree.unpin(exiting.version);
                self.queue_free();
            }
            self.answered += 1;
        }
        let Some(reading) = self.reads.pop_front() else { return };
        // An exit read goes back to the front until its last step answers it.
        let under_way = match reading {
            Reading::Read(Read::Account(account, reply)) => {
                let proof: Option<AccountProof> =
                    self.tree.released().and_then(|version| self.tree.account_proof(version, account));
                let _ = reply.send(proof);
                None
            }
            Reading::Read(Read::Entitlement(account, reply)) => {
                let answer =
                    self.tree.released().and_then(|version| self.claims.entitlement(&self.tree, version, account));
                let _ = reply.send(answer);
                None
            }
            Reading::Read(Read::Distribution(account, reply)) => {
                let answer =
                    self.tree.released().and_then(|version| self.claims.distribution(&self.tree, version, account));
                let _ = reply.send(answer);
                None
            }
            Reading::Read(Read::Exit(account, reply)) => {
                let head = self.tree.released().and_then(|version| {
                    let proof = self.tree.exit_head(version, account)?;
                    Some((version.id(), proof))
                });
                match head {
                    Some((version, proof)) => {
                        self.tree.pin(version);
                        let exiting = Exiting { version, account, proof, last: None, listed: false, next: 1, reply };
                        Some(Reading::Exit(Box::new(exiting)))
                    }
                    None => {
                        let _ = reply.send(None);
                        None
                    }
                }
            }
            Reading::Exit(mut exiting) => {
                if self.exit_step(&mut exiting) {
                    Some(Reading::Exit(exiting))
                } else {
                    let Exiting { version, proof, reply, .. } = *exiting;
                    self.tree.unpin(version);
                    self.queue_free();
                    let _ = reply.send(Some(proof));
                    None
                }
            }
        };
        match under_way {
            Some(reading) => self.reads.push_front(reading),
            None => self.answered += 1,
        }
    }

    /// Proves up to `PROOF_STEP` of an exit's items, its queued requests, pending deposits and credits
    /// in proof order, each listed from the last one proved, then its approvals, and returns whether any
    /// are left.
    fn exit_step(&self, exiting: &mut Exiting) -> bool {
        let version = self.tree.version(exiting.version).expect("a pinned version is kept");
        let (account, proof) = (exiting.account, &mut exiting.proof);
        let nonces = proof.account.as_ref().map_or(0, |held| held.leaf.withdrawal_nonce);
        for _ in 0..PROOF_STEP {
            let item = if exiting.listed { None } else { self.tree.next_exit_item(account, exiting.last.as_ref()) };
            exiting.listed = item.is_none();
            if let Some(item) = item {
                self.tree.prove_exit_item(version, account, &item, proof);
                exiting.last = Some(item);
            } else if exiting.next <= nonces {
                proof.approvals.extend(self.tree.approval_proof(version, account, exiting.next));
                exiting.next += 1;
            } else {
                return false;
            }
        }
        true
    }

    /// The oldest background job's next step.
    fn background_step(&mut self) {
        match self.background.pop_front().expect("next_lane picks a lane that holds a step") {
            Background::Free => {
                self.tree.collect(FREE_STEP);
                if self.tree.garbage() > 0 {
                    self.background.push_front(Background::Free);
                }
            }
            Background::Capture { version, mut accounts } => {
                let tree_version = self.tree.version(version).expect("a pinned version is kept");
                let rest = accounts.split_off(accounts.len().saturating_sub(PROOF_STEP));
                for account in rest {
                    self.claims.capture(&self.tree, tree_version, account);
                }
                if accounts.is_empty() {
                    self.tree.unpin(version);
                    self.queue_free();
                } else {
                    self.background.push_front(Background::Capture { version, accounts });
                }
            }
            Background::Prove { version, seq, mut approvals, mut proofs } => {
                let tree_version = self.tree.version(version).expect("a pinned version is kept");
                for _ in 0..PROOF_STEP {
                    let Some((account, nonce)) = approvals.pop_front() else { break };
                    // As the inline path asserts: only a version gone, not one held, leaves a payout unproved.
                    let proof = self.tree.approval_proof(tree_version, account, nonce);
                    proofs
                        .push(((account, nonce), Some(proof.expect("a checkpoint includes every preceding approval"))));
                }
                if approvals.is_empty() {
                    self.tree.unpin(version);
                    self.queue_free();
                    self.outbox.push(Outcome::Proved { seq, proofs });
                } else {
                    self.background.push_front(Background::Prove { version, seq, approvals, proofs });
                }
            }
        }
    }

    /// Queues freeing, once, when the tree holds nodes no version does.
    fn queue_free(&mut self) {
        if self.tree.garbage() > 0 && !self.background.iter().any(|job| matches!(job, Background::Free)) {
            self.background.push_back(Background::Free);
        }
    }

    /// Ends every read and job that holds a version of a tree about to be replaced: exit reads answer
    /// none, and payout proofs come back unproved, for a later checkpoint to prove.
    fn cancel(&mut self) {
        for reading in std::mem::take(&mut self.reads) {
            match reading {
                Reading::Exit(exiting) => {
                    let _ = exiting.reply.send(None);
                    self.answered += 1;
                }
                Reading::Read(read) => self.reads.push_back(Reading::Read(read)),
            }
        }
        for job in std::mem::take(&mut self.background) {
            if let Background::Prove { seq, approvals, mut proofs, .. } = job {
                proofs.extend(approvals.into_iter().map(|key| (key, None)));
                self.outbox.push(Outcome::Proved { seq, proofs });
            }
        }
    }
}

#[cfg(test)]
mod tests;
