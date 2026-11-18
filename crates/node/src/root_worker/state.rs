//! The root worker's logic, apart from its thread: what it holds, and each step it takes. No step
//! waits on anything, and each takes about a millisecond at most, but a commit's, whose size the
//! sequencer bounds before it sends one.

use std::collections::VecDeque;

use engine_core::merkle::tree::StateTree;
use engine_core::merkle::{AccountProof, ExitProof};
use engine_core::types::AccountId;
use tracing::debug;

use crate::root_worker::claims::Claims;
use crate::root_worker::protocol::{Lane, Progress, Word};
use crate::root_worker::{Outcome, Read, Work};

/// Nodes freed in one step.
const FREE_STEP: usize = 20_000;
/// Proofs taken in one step of a claim capture, a payout's proofs, or an exit read's approvals.
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
}

/// A read in progress.
enum Reading {
    Read(Read),
    /// An exit proof whose approvals are read a step at a time, against the version it pinned.
    Exit {
        version: u64,
        account: AccountId,
        proof: Box<ExitProof>,
        next: u64,
        reply: tokio::sync::oneshot::Sender<Option<ExitProof>>,
    },
}

/// What no root waits for.
enum Background {
    /// Nodes no version holds any more.
    Free,
    /// The watched accounts' claim proofs at a version just committed, which they pin.
    Capture { version: u64, accounts: Vec<AccountId> },
    /// A payout's first proofs at the version of the checkpoint at `seq`, which they pin.
    Prove { version: u64, seq: u64, approvals: VecDeque<(AccountId, u64)>, proofs: Vec<super::Proved> },
}

impl WorkerState {
    pub(super) fn new(tree: StateTree) -> Self {
        Self {
            tree,
            work: VecDeque::new(),
            reads: VecDeque::new(),
            background: VecDeque::new(),
            claims: Claims::default(),
            last: Lane::Background,
            outbox: Vec::new(),
        }
    }

    /// Queues a message in its lane. A tree handed over with nothing queued ahead of it, a release whose
    /// version is committed, and the vault's latest checkpoint apply at once, so every read the
    /// sequencer forwards after them sees them: the sequencer sends a release only once the commit it
    /// names has answered, or right after the tree that holds its version.
    pub(super) fn take(&mut self, message: super::Message) {
        match message {
            super::Message::Work(Work::Install(tree)) if self.work.is_empty() && self.background.is_empty() => {
                self.cancel();
                self.tree = *tree;
            }
            super::Message::Work(Work::Release(seq, root)) if self.tree.release(seq, root) => self.queue_free(),
            // Claims read nothing of the tree's, so the vault's checkpoint counts for every claim read
            // forwarded after it.
            super::Message::Work(Work::Accept(latest)) => self.claims.accept(latest),
            super::Message::Work(work) => self.work.push_back(work),
            super::Message::Read(read) => self.reads.push_back(Reading::Read(read)),
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
            Lane::Work => self.work_step(),
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

    /// The sequencer's next message, in the order it sent them, returning the leaf writes it finished.
    fn work_step(&mut self) -> u64 {
        match self.work.pop_front().expect("next_lane picks a lane that holds a step") {
            Work::Install(tree) => {
                self.cancel();
                self.tree = *tree;
            }
            Work::Advance(delta, units) => {
                delta.apply(&mut self.tree);
                return units;
            }
            Work::Reset => self.tree.reset(),
            Work::Discard => {
                self.cancel();
                self.tree = StateTree::default();
            }
            Work::Commit(inputs, reply) => {
                let answer = inputs.apply(&mut self.tree);
                // A sequencer that stopped waiting has stopped: its thread sees the closed channel.
                let _ = reply.send(answer);
                let version = self.tree.newest().expect("committed above").id();
                let accounts = self.claims.watched();
                if !accounts.is_empty() {
                    self.tree.pin(version);
                    self.background.push_back(Background::Capture { version, accounts });
                }
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
                self.background.push_back(Background::Prove { version, seq, approvals, proofs: Vec::new() });
            }
            Work::Accept(latest) => self.claims.accept(latest),
        }
        0
    }

    /// The oldest read's next step, against the newest released version.
    fn read_step(&mut self) {
        match self.reads.pop_front().expect("next_lane picks a lane that holds a step") {
            Reading::Read(Read::Account(account, reply)) => {
                let proof: Option<AccountProof> =
                    self.tree.released().and_then(|version| self.tree.account_proof(version, account));
                let _ = reply.send(proof);
            }
            Reading::Read(Read::Entitlement(account, reply)) => {
                let answer =
                    self.tree.released().and_then(|version| self.claims.entitlement(&self.tree, version, account));
                let _ = reply.send(answer);
            }
            Reading::Read(Read::Distribution(account, reply)) => {
                let answer =
                    self.tree.released().and_then(|version| self.claims.distribution(&self.tree, version, account));
                let _ = reply.send(answer);
            }
            Reading::Read(Read::Exit(account, reply)) => {
                let proof = self.tree.released().and_then(|version| {
                    let proof = self.tree.exit_proof_without_approvals(version, account)?;
                    Some((version.id(), proof))
                });
                let Some((version, proof)) = proof else {
                    let _ = reply.send(None);
                    return;
                };
                self.tree.pin(version);
                self.reads.push_front(Reading::Exit { version, account, proof: Box::new(proof), next: 1, reply });
            }
            Reading::Exit { version, account, mut proof, next, reply } => {
                let tree_version = self.tree.version(version).expect("a pinned version is kept");
                let nonces = proof.account.as_ref().map_or(0, |held| held.leaf.withdrawal_nonce);
                let last = nonces.min(next.saturating_add(PROOF_STEP as u64 - 1));
                let approvals =
                    (next..=last).filter_map(|nonce| self.tree.approval_proof(tree_version, account, nonce));
                proof.approvals.extend(approvals);
                if last < nonces {
                    return self.reads.push_front(Reading::Exit { version, account, proof, next: last + 1, reply });
                }
                self.tree.unpin(version);
                self.queue_free();
                let _ = reply.send(Some(*proof));
            }
        }
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
                    proofs.push(((account, nonce), self.tree.approval_proof(tree_version, account, nonce)));
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
                Reading::Exit { reply, .. } => _ = reply.send(None),
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
