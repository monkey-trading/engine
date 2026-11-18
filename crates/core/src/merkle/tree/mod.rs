//! The state tree in versions over one arena. A working version takes leaf writes, in as many
//! batches as its owner likes, and each commit makes it a new version: the tree of a root, which
//! proofs read. Versions share every node neither changed, so a version costs only what changed
//! since the one before it. A version is kept until a later one is released, as the version of a
//! checkpoint the log holds, and no read holds it; its own nodes are then freed. An engine that
//! computes its own roots commits and releases each at once, and so keeps one version
//! (`Engine::state_root`); a node that computes roots on another thread keeps the tree there, fed
//! the leaf writes the engine's changes make (`Engine::take_root_delta`, `Engine::commit_inputs`).

mod arena;
mod proofs;
mod trie;
pub(crate) mod value;

use std::collections::VecDeque;

use alloy_primitives::B256;

use crate::merkle::tree::arena::{Arena, NodeRef};
use crate::merkle::tree::trie::NewLeaf;
use crate::merkle::tree::value::{Association, ExitIndex, ExitTotals, LeafValue};
use crate::merkle::{ExitTerms, RootWork, exit_terms_hash, exit_terms_key, leaf_hash};
use crate::protocol::ProtocolVersion;

pub use value::LeafUpdate;

/// Branches this many levels above the deepest leaves of a balanced tree are hashed only on commit,
/// since nearly every batch of writes changes them.
const BAND_BELOW_TOP: u32 = 8;

/// The state tree's versions, oldest first, and its working version.
#[derive(Debug, Default)]
pub struct StateTree {
    arena: Arena,
    working: NodeRef,
    /// The exit totals of the working version.
    totals: ExitTotals,
    /// What the working version's writes have hashed since the last commit.
    work: RootWork,
    versions: VecDeque<Version>,
    next_id: u64,
    index: ExitIndex,
    /// Index entries the working version made since the last commit, and those it dropped.
    added: Vec<Association>,
    dropped: Vec<Association>,
}

/// One committed version: the tree of a root.
#[derive(Debug)]
pub struct Version {
    id: u64,
    root: NodeRef,
    hash: B256,
    seq: u64,
    protocol: ProtocolVersion,
    work: RootWork,
    totals: ExitTotals,
    released: bool,
    pins: u32,
    /// Index entries this version holds that the next one dropped, pruned once this version goes
    /// unless a version kept, or the working one, still holds them.
    dropped: Vec<Association>,
}

/// What the exit terms leaf commits beside the totals: `Engine::exit_deposit_frontier`'s cursor,
/// gaps, unheld principal and held receipts, and the finalised frontier's deposit count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermsInput {
    pub(crate) frontier: (u64, Vec<u64>, crate::Fixed, crate::Fixed),
    pub(crate) deposit_base: u64,
}

impl Default for NodeRef {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl Version {
    /// The seq of the last command the root covers.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The root.
    pub fn root(&self) -> B256 {
        self.hash
    }

    /// The version's id among the tree's versions, for `StateTree::pin`.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// What computing the root hashed.
    pub fn work(&self) -> RootWork {
        self.work
    }

    /// Whether a checkpoint the log holds released it.
    pub fn released(&self) -> bool {
        self.released
    }
}

impl StateTree {
    /// Writes `updates` into the working version, each key at most once, and hashes the stale branches
    /// below the band only commits hash. A write that leaves a leaf as it was changes nothing. Into an
    /// empty tree, the writes are built bottom up at once.
    pub fn advance(&mut self, updates: Vec<LeafUpdate>) {
        self.write(updates);
        let band = self.band();
        self.work.nodes += self.arena.rehash(self.working, 0, band);
    }

    /// Writes `updates` into the working version without hashing.
    fn write(&mut self, mut updates: Vec<LeafUpdate>) {
        updates.sort_unstable_by_key(|update| update.key);
        // The order of two writes of one key would decide which stands.
        assert!(updates.windows(2).all(|pair| pair[0].key != pair[1].key), "a batch writes each key once");
        self.work.leaves += updates.len();
        if self.working.is_empty() {
            return self.build(updates);
        }
        for LeafUpdate { key, value_hash, value } in updates {
            let new = value_hash.map(|value_hash| NewLeaf { hash: leaf_hash(key, value_hash), value });
            let old = self.arena.lookup(self.working, &key);
            let old_value = old.and_then(|leaf| leaf.value.as_deref());
            let new_value = new.as_ref().and_then(|leaf| leaf.value.as_deref());
            let rehash = old.map(|leaf| leaf.hash) != new.as_ref().map(|leaf| leaf.hash);
            if !rehash && old_value == new_value {
                continue;
            }
            let old_exit = old_value.filter(|value| value.is_exit()).cloned();
            self.note_exit(old_exit.as_ref(), new.as_ref().and_then(|leaf| leaf.value.as_deref()));
            self.working = self.arena.set(self.working, &key, new, rehash);
        }
    }

    /// Builds the working version, which is empty, over `updates`, sorted by key.
    fn build(&mut self, updates: Vec<LeafUpdate>) {
        let leaves: Vec<(B256, NewLeaf)> = updates
            .into_iter()
            .filter_map(|LeafUpdate { key, value_hash, value }| {
                value_hash.map(|value_hash| (key, NewLeaf { hash: leaf_hash(key, value_hash), value }))
            })
            .collect();
        for (_, leaf) in &leaves {
            self.note_exit(None, leaf.value.as_deref());
        }
        let (root, hashed) = self.arena.build(leaves);
        self.working = root;
        self.work.nodes += hashed;
    }

    /// Keeps the exit totals and index with a leaf going from `old` to `new`.
    fn note_exit(&mut self, old: Option<&LeafValue>, new: Option<&LeafValue>) {
        LeafValue::shift_totals(old, new, &mut self.totals);
        let (old, new) = (old.and_then(LeafValue::association), new.and_then(LeafValue::association));
        if old == new {
            return;
        }
        if let Some(old) = old {
            self.dropped.push(old);
        }
        if let Some(new) = new {
            self.index.insert(new.clone());
            self.added.push(new);
        }
    }

    /// Commits the working version as the root of the state after the command at `seq`, under
    /// `protocol`, with the exit terms leaf from `terms` and the working totals when given. The new
    /// version is unreleased. Returns the root and what computing it hashed since the last commit.
    pub fn commit(&mut self, seq: u64, protocol: ProtocolVersion, terms: Option<TermsInput>) -> (B256, RootWork) {
        if let Some(TermsInput { frontier: (deposit_cursor, gaps, unheld, held_receipts), deposit_base }) = terms {
            let terms = ExitTerms {
                deposit_cursor,
                deposit_base,
                pending: self.totals.pending,
                held: self.totals.held,
                unheld,
                held_receipts,
                gaps,
            };
            let hash = exit_terms_hash(&terms);
            self.write(vec![LeafUpdate::valued(exit_terms_key(), Some((hash, LeafValue::Terms(terms))))]);
        }
        self.work.nodes += self.arena.rehash(self.working, 0, 0);
        let hash = self.arena.hash_of(self.working);
        self.arena.retain(self.working);
        let dropped = std::mem::take(&mut self.dropped);
        match self.versions.back_mut() {
            Some(previous) => previous.dropped = dropped,
            // Entries the working version both made and dropped since it was empty.
            None => {
                let gone: Vec<Association> = dropped.into_iter().filter(|entry| !self.holds(entry)).collect();
                gone.iter().for_each(|entry| self.index.remove(entry));
            }
        }
        self.added.clear();
        let work = std::mem::take(&mut self.work);
        self.next_id += 1;
        self.versions.push_back(Version {
            id: self.next_id,
            root: self.working,
            hash,
            seq,
            protocol,
            work,
            totals: self.totals,
            released: false,
            pins: 0,
            dropped: Vec::new(),
        });
        (hash, work)
    }

    /// Releases the newest version at `seq` with root `root`, as the version of a checkpoint the log
    /// holds, and lets every older version go once no read holds it. Returns whether there was one.
    pub fn release(&mut self, seq: u64, root: B256) -> bool {
        let Some(at) = self.versions.iter().rposition(|v| v.seq == seq && v.hash == root) else { return false };
        self.versions[at].released = true;
        self.drop_superseded();
        true
    }

    /// Marks every version unreleased, for a tree handed to an owner that releases each version only
    /// once a checkpoint the log holds names its root.
    pub(crate) fn withhold(&mut self) {
        self.versions.iter_mut().for_each(|version| version.released = false);
    }

    /// Drops every unpinned version older than the newest released one.
    fn drop_superseded(&mut self) {
        let Some(newest) = self.versions.iter().rposition(|v| v.released) else { return };
        let mut at = 0;
        let mut newest = newest;
        while at < newest {
            if self.versions[at].pins > 0 {
                at += 1;
                continue;
            }
            let version = self.versions.remove(at).expect("in range");
            newest -= 1;
            self.arena.release(version.root);
            for entry in version.dropped {
                if !self.holds(&entry) {
                    self.index.remove(&entry);
                }
            }
        }
    }

    /// Whether a version kept, or the working one, holds `entry`.
    fn holds(&self, entry: &Association) -> bool {
        let key = entry.key();
        let names = |root: NodeRef| {
            let leaf = self.arena.lookup(root, &key);
            leaf.and_then(|leaf| leaf.value.as_ref()).and_then(|value| value.association()).as_ref() == Some(entry)
        };
        names(self.working) || self.versions.iter().any(|version| names(version.root))
    }

    /// Discards the working version's writes since the last commit: the working version is the
    /// newest committed one again, or empty with none.
    pub fn reset(&mut self) {
        let (root, totals) =
            self.versions.back().map_or((NodeRef::EMPTY, ExitTotals::default()), |v| (v.root, v.totals));
        self.arena.retain(root);
        self.arena.release(std::mem::replace(&mut self.working, root));
        self.totals = totals;
        self.work = RootWork::default();
        self.dropped.clear();
        for entry in std::mem::take(&mut self.added) {
            if !self.holds(&entry) {
                self.index.remove(&entry);
            }
        }
    }

    /// Frees up to `budget` nodes no version holds any more, and returns how many it freed.
    pub fn collect(&mut self, budget: usize) -> usize {
        self.arena.collect(budget)
    }

    /// Nodes waiting for `collect`.
    pub fn garbage(&self) -> usize {
        self.arena.dead()
    }

    /// Keeps the version `id` from being dropped until `unpin`, for a read over several steps.
    pub fn pin(&mut self, id: u64) {
        self.versions.iter_mut().find(|v| v.id == id).expect("a pinned version is kept").pins += 1;
    }

    /// Ends a `pin`, dropping the version if a newer one was released meanwhile.
    pub fn unpin(&mut self, id: u64) {
        let version = self.versions.iter_mut().find(|v| v.id == id).expect("a pinned version is kept");
        version.pins = version.pins.checked_sub(1).expect("an unpinned version was pinned");
        self.drop_superseded();
    }

    /// The newest released version.
    pub fn released(&self) -> Option<&Version> {
        self.versions.iter().rev().find(|v| v.released)
    }

    /// The newest version, released or not.
    pub fn newest(&self) -> Option<&Version> {
        self.versions.back()
    }

    /// The version `id`, if it is kept.
    pub fn version(&self, id: u64) -> Option<&Version> {
        self.versions.iter().find(|v| v.id == id)
    }

    /// The newest version at `seq`, released or not.
    pub fn at_seq(&self, seq: u64) -> Option<&Version> {
        self.versions.iter().rev().find(|v| v.seq == seq)
    }

    /// The depth from which `advance` hashes stale branches: `BAND_BELOW_TOP` levels above the deepest
    /// leaves of a balanced tree over the leaves held.
    fn band(&self) -> usize {
        let (_, leaves) = self.arena.live();
        leaves.max(1).ilog2().saturating_sub(BAND_BELOW_TOP) as usize
    }

    /// The branches and leaves the arena holds, for tests and memory figures.
    pub fn nodes(&self) -> (usize, usize) {
        self.arena.live()
    }
}

#[cfg(test)]
mod tests;
