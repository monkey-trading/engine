//! The engine's state tree: the leaf writes each root takes from what changed since the last one, the
//! leaf of the engine's remaining state, and the tree a node computes roots with on another thread.
//!
//! An engine computes its own roots inline (`Engine::state_root`): it keeps the tree, writes every
//! leaf changed since the last root into it, and commits it, keeping one version, which its proofs
//! read. A node can instead take the tree away (`Engine::detach_tree`) and feed it the same leaf
//! writes: in parts as commands run (`Engine::take_root_delta`) and the rest at a root
//! (`Engine::commit_inputs`). Each part is read from the state as it is when taken, and a key changed
//! after that is recorded again, so the root a commit gives is the inline root of the same state,
//! whatever parts went before it. A protocol version change between parts resets the working version
//! and puts the keys sent since the last root back in the record, or, where an activation changes
//! leaves no command touched, discards the tree (`Engine::take_tree_event`).

use alloy_primitives::B256;
use sha3::{Digest, Keccak256};

mod globals;
mod leaves;
mod touched;
use crate::engine::{Engine, Markout};
use crate::merkle::tree::{LeafUpdate, StateTree, TermsInput};
use crate::merkle::{globals_key, json_hash};
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId, OrderId};
pub(crate) use touched::Changed;
pub use touched::RootWork;
use touched::Touched;

/// What a detached tree's owner must do before the next leaf writes, after a protocol version change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeEvent {
    /// Discard the working version's writes since the last commit: the keys sent since then are in
    /// the record again, to be read under the new version.
    Reset,
    /// Discard every version: the activation changed leaves no command touched, and every key is in
    /// the record again for a tree built afresh.
    Discard,
}

/// The engine's side of a detached tree.
#[derive(Debug, Default)]
pub(crate) struct Detached {
    /// The keys sent in parts since the last root.
    drained: Touched,
    /// Whether an activation applying now discards the tree.
    discard: bool,
    event: Option<TreeEvent>,
}

/// Leaf writes taken from the engine between roots.
#[derive(Debug)]
pub struct RootDelta {
    updates: Vec<LeafUpdate>,
}

/// The leaf writes a root takes at once: every key still in the record, the markets, the leaf of the
/// remaining state, and the exit terms.
#[derive(Debug)]
pub struct CommitInputs {
    updates: Vec<LeafUpdate>,
    seq: u64,
    protocol: ProtocolVersion,
    terms: Option<TermsInput>,
}

impl RootDelta {
    /// How many leaf writes the part holds.
    pub fn len(&self) -> usize {
        self.updates.len()
    }

    /// Whether the part writes nothing.
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }

    /// Writes the part into `tree`'s working version.
    pub fn apply(self, tree: &mut StateTree) {
        tree.advance(self.updates);
    }
}

impl CommitInputs {
    /// The seq of the command the root follows.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// How many leaf writes the root takes.
    pub fn len(&self) -> usize {
        self.updates.len()
    }

    /// Whether the root takes no leaf write.
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }

    /// Writes the inputs into `tree`'s working version and commits it, returning the root and what
    /// computing it hashed since the last commit.
    pub fn apply(self, tree: &mut StateTree) -> (B256, RootWork) {
        let CommitInputs { updates, seq, protocol, terms } = self;
        tree.advance(updates);
        tree.commit(seq, protocol, terms)
    }
}

impl Engine {
    /// The keccak-256 commitment to the whole state, posted on-chain at checkpoints; anyone
    /// replaying the log must get the same root. From `V9`, the root of the state tree (`merkle`),
    /// which rehashes only what changed since the last root; before, the hash of the canonical state
    /// encoding.
    pub fn state_root(&mut self) -> [u8; 32] {
        if self.protocol_version >= ProtocolVersion::V9 {
            return self.merkle_root().0;
        }
        self.clear_touched();
        // Every field is a map, list, string, or integer, all of which serde_json encodes.
        let bytes = serde_json::to_vec(self).expect("engine state serialises");
        Keccak256::digest(bytes).into()
    }

    /// The root of the state tree, rehashing the leaves changed since the last root. With no tree
    /// yet, as on `V9`'s first root, every leaf is hashed. The tree keeps this root's version alone.
    pub(crate) fn merkle_root(&mut self) -> B256 {
        // The node computes a detached tree's roots, and a root here would rebuild every leaf.
        assert!(self.detached.is_none(), "an engine whose tree is detached computes no root");
        let touched = self.take_touched();
        let touched = if self.tree.is_some() { touched } else { self.every_key() };
        let inputs = self.inputs(&touched);
        let seq = inputs.seq;
        let tree = self.tree.get_or_insert_with(StateTree::default);
        let (root, _) = inputs.apply(tree);
        tree.release(seq, root);
        tree.collect(usize::MAX);
        root
    }

    /// The leaf writes a root takes from `touched`, with the leaf of the remaining state and, from
    /// `V38`, the exit terms.
    fn inputs(&mut self, touched: &Touched) -> CommitInputs {
        let protocol = self.protocol_version;
        let terms = (protocol >= ProtocolVersion::V38).then(|| TermsInput {
            frontier: self.exit_deposit_frontier(protocol),
            deposit_base: self.custody_checkpoint.map_or(0, |c| c.deposit_count),
        });
        let mut updates = self.leaf_updates(touched);
        updates.push(LeafUpdate::hashed(globals_key(), Some(json_hash(&self.globals()))));
        // A seq has been applied: `V9` takes effect through a sequenced `ActivateProtocol`.
        let seq = self.last_seq.expect("a state root under V9 follows a command");
        CommitInputs { updates, seq, protocol, terms }
    }

    /// Hands the state tree to the caller, which computes this engine's roots from here on from
    /// `take_root_delta` and `commit_inputs`; `state_root` refuses after it. Its version is handed over
    /// unreleased: the caller releases it only once it knows a checkpoint holds its root. `None` with
    /// no tree to hand over: before the first root under `V9`, or after a decode.
    pub fn detach_tree(&mut self) -> Option<StateTree> {
        let mut tree = self.tree.take()?;
        tree.withhold();
        self.detached = Some(Detached::default());
        Some(tree)
    }

    /// Up to `entries` keys from the record of what changed since the last root, as leaf writes read
    /// from the state now, for a detached tree's working version. The markets, which every price
    /// changes, wait for the root.
    pub fn take_root_delta(&mut self, entries: usize) -> RootDelta {
        let part = self.take_touched_part(entries);
        let updates = self.leaf_updates(&part);
        self.detached.as_mut().expect("parts feed a detached tree").drained.absorb(part);
        RootDelta { updates }
    }

    /// The leaf writes of a detached tree's root after the last command: every key still in the
    /// record, the markets, the remaining state's leaf and the exit terms.
    pub fn commit_inputs(&mut self) -> CommitInputs {
        let touched = self.take_touched();
        self.detached.as_mut().expect("commits feed a detached tree").drained = Touched::default();
        self.inputs(&touched)
    }

    /// How many keys the record of what changed since the last root holds: the root work left before
    /// a detached tree's next commit, beside what its owner has yet to write.
    pub fn root_backlog(&self) -> usize {
        self.changed_key_count()
    }

    /// What a protocol version change asks of a detached tree's owner before any later leaf write,
    /// clearing it.
    pub fn take_tree_event(&mut self) -> Option<TreeEvent> {
        self.detached.as_mut().and_then(|detached| detached.event.take())
    }

    /// Discards the tree, for an activation that changes leaves no command touched: the next root
    /// hashes every leaf. With the tree detached, its owner is told when the version changes.
    pub(crate) fn discard_tree(&mut self) {
        match &mut self.detached {
            Some(detached) => detached.discard = true,
            None => self.tree = None,
        }
    }

    /// Readies a detached tree's next leaf writes for the protocol version just set: every key, after
    /// a discarding activation, or the keys sent since the last root, is in the record again.
    pub(crate) fn tree_version_changed(&mut self) {
        let Some(detached) = &mut self.detached else { return };
        let drained = std::mem::take(&mut detached.drained);
        if std::mem::take(&mut detached.discard) {
            detached.event = Some(TreeEvent::Discard);
            let every = self.every_key();
            self.record_touched(every);
        } else {
            // A discard waiting to be taken stands: it covers a reset.
            detached.event = Some(detached.event.unwrap_or(TreeEvent::Reset));
            self.record_touched(drained);
        }
    }

    /// Clears the record of what changed since the last root, for a root that does not use it.
    pub(crate) fn clear_touched(&mut self) {
        self.take_touched();
    }

    /// What the last state root hashed, or `None` before the first root under `V9`.
    pub fn last_root_work(&self) -> Option<RootWork> {
        self.tree.as_ref().and_then(StateTree::newest).map(|version| version.work())
    }

    /// Records that `account`'s position in `market` changed, for the state root.
    pub(crate) fn record_position(&mut self, account: AccountId, market: MarketId) {
        self.index_position(account, market);
        self.changed.positions.insert((account, market));
    }

    /// Records that `account`'s resting order `order_id` was added or removed, for the state root.
    pub(crate) fn record_order(&mut self, account: AccountId, order_id: OrderId) {
        self.changed.orders.insert((account, order_id));
    }

    /// Records a split-normalized pending markout for its committed leaf.
    pub(crate) fn record_markout_change(&mut self, offset: usize) {
        self.changed.markouts.insert(self.markout_base + offset as u64);
    }

    /// Queues a markout, recording its index for the state root.
    pub(crate) fn queue_markout(&mut self, markout: Markout) {
        self.markouts.push_back(markout);
        self.changed.markouts.insert(self.markout_base + self.markouts.len() as u64 - 1);
    }

    /// Queues a markout in due order, after every markout due no later, recording for the state root
    /// the index of each markout it moves.
    pub(crate) fn insert_markout(&mut self, markout: Markout) {
        let at = self.markouts.partition_point(|queued| queued.due <= markout.due);
        self.markouts.insert(at, markout);
        let base = self.markout_base;
        self.changed.markouts.extend((at..self.markouts.len()).map(|position| base + position as u64));
    }

    /// Puts back at the front of the queue a markout `pop_markout` took in the same pass, recording
    /// its index for the state root.
    pub(crate) fn requeue_markout(&mut self, markout: Markout) {
        self.markouts.push_front(markout);
        if self.protocol_version >= ProtocolVersion::V9 {
            // `pop_markout` moved the index up when it took this markout.
            self.markout_base = self.markout_base.checked_sub(1).expect("a requeued markout was popped first");
        }
        self.changed.markouts.insert(self.markout_base);
    }

    /// Takes the markout at the front of the queue, recording its index for the state root. From
    /// `V9` the next markout's index moves up by one; earlier versions keep every index at 0, which
    /// their encoding leaves out.
    pub(crate) fn pop_markout(&mut self) -> Option<Markout> {
        let markout = self.markouts.pop_front()?;
        self.changed.markouts.insert(self.markout_base);
        if self.protocol_version >= ProtocolVersion::V9 {
            self.markout_base += 1;
        }
        Some(markout)
    }

    /// The queued markout at `index`, if it is still queued.
    fn markout(&self, index: u64) -> Option<&Markout> {
        let position = index.checked_sub(self.markout_base)?;
        self.markouts.get(usize::try_from(position).ok()?)
    }
}

/// Whether a persisted index is zero, so its default stays out of earlier state encodings.
pub(crate) fn is_zero(index: &u64) -> bool {
    *index == 0
}
