//! The state tree. From protocol `V9`, the state root is the root of a binary sparse Merkle tree
//! over the engine's state, hashed with keccak-256 so that a contract can check a proof against a
//! posted root. A command changes a few entries, and the next root rehashes only those entries and
//! the nodes above them.
//!
//! Layout, in Solidity terms:
//! - **Keys.** Each entry has a 32-byte key, `keccak256(abi.encodePacked(uint8 kind, id))`, where
//!   kind 0 is the engine's remaining state (no id), 1 an account (`address`), 2 a market
//!   (`uint32`), 3 an account's request nonce (`address`), 4 a credited custody reference (its
//!   UTF-8 bytes), 5 a pending markout (its `uint64` index; see `Engine::markout_base`), 6 a
//!   resting order (its `uint64` id), 7 a position (`address` account, `uint32` market), and from
//!   `V19` 8 an account's emission entitlement (`address`) and 9 its emission period result
//!   (`address`).
//! - **Paths.** The key's bits, most significant first, are the path from the root: bit `d`,
//!   `(uint256(key) >> (255 - d)) & 1`, picks the child at depth `d + 1`, 0 left and 1 right.
//! - **Leaves.** `keccak256(abi.encodePacked(bytes1(0x00), key, valueHash))`; each kind's
//!   `valueHash` is in `leaf.rs`.
//! - **Nodes.** `keccak256(abi.encodePacked(bytes1(0x01), left, right))`. The distinct first bytes
//!   keep a leaf from passing as a node, and a node as a leaf.
//! - **Empty subtree.** `bytes32(0)`.
//! - **Single leaf.** A subtree holding one leaf hashes to that leaf, so each leaf sits at the depth
//!   where its key parts from every other key, and its proof has that many siblings.
//! - **Proofs.** The sibling hashes from the root down to the leaf: `siblings[d]` is the subtree at
//!   depth `d + 1` on the side the key does not take. To check one, hash the leaf, then for `d`
//!   from `siblings.length - 1` down to 0 hash it with `siblings[d]`, the leaf's side given by bit
//!   `d`, and compare the result with the root ([`verify`]).
//! - **Accounts.** An account's proof holds only when it carries exactly the account leaf's
//!   `openPositions` position leaves, in strictly increasing, and so distinct, markets, each with
//!   its market's leaf for the mark ([`AccountProof::verify`]). Checking the count alone would let
//!   one position stand in twice for another, hiding it.
//!
//! The tree lives in memory beside the engine state and is not part of its encoding: an engine
//! rebuilt from its encoding, or by replaying the log, rebuilds the tree at its first root.

mod leaf;
pub use leaf::*;
mod proof;
pub use proof::*;
pub(crate) mod state;
pub use state::RootWork;

use std::collections::BTreeMap;

use alloy_primitives::B256;
use sha3::{Digest, Keccak256};

/// The hash of an empty subtree.
pub const EMPTY: B256 = B256::ZERO;
/// The first byte hashed into a leaf.
const LEAF_PREFIX: u8 = 0x00;
/// The first byte hashed into a node.
const NODE_PREFIX: u8 = 0x01;

/// What a subtree holds.
enum Subtree {
    Empty,
    /// One leaf, by its hash.
    Leaf(B256),
    /// Two or more leaves, under a node.
    Node,
}

/// A sparse Merkle tree over 256-bit keys, laid out as the module docs describe.
#[derive(Debug, Default)]
pub(crate) struct SparseMerkleTree {
    /// Each leaf's hash, by key.
    leaves: BTreeMap<B256, B256>,
    /// The hash of each subtree holding two or more leaves, by its depth and the key prefix that
    /// leads to it, with the bits from that depth on cleared.
    nodes: BTreeMap<(usize, B256), B256>,
}

impl SparseMerkleTree {
    /// The root hash.
    pub(crate) fn root(&self) -> B256 {
        self.hash_of(0, B256::ZERO)
    }

    /// Sets the leaf of each key to its value hash, or removes it for `None`, then rehashes the
    /// nodes above the leaves that changed. Returns how many node hashes that took.
    pub(crate) fn update(&mut self, changes: BTreeMap<B256, Option<B256>>) -> usize {
        let changed: Vec<B256> = changes
            .into_iter()
            .filter_map(|(key, value_hash)| {
                let leaf = value_hash.map(|value_hash| leaf_hash(key, value_hash));
                let old = match leaf {
                    Some(leaf) => self.leaves.insert(key, leaf),
                    None => self.leaves.remove(&key),
                };
                (old != leaf).then_some(key)
            })
            .collect();
        let mut hashed = 0;
        if !changed.is_empty() {
            self.rehash(0, B256::ZERO, &changed, &mut hashed);
        }
        hashed
    }

    /// The siblings proving the leaf at `key`, from the root down, or `None` when there is no leaf
    /// at `key`.
    pub(crate) fn proof(&self, key: B256) -> Option<Vec<B256>> {
        self.leaves.get(&key)?;
        let mut siblings = Vec::new();
        while matches!(self.subtree(siblings.len(), prefix(&key, siblings.len())), Subtree::Node) {
            let depth = siblings.len();
            let base = prefix(&key, depth);
            let sibling = if bit(&key, depth) { base } else { with_bit(base, depth) };
            siblings.push(self.hash_of(depth + 1, sibling));
        }
        Some(siblings)
    }

    /// Rehashes the subtree at `depth` under `prefix`, given the sorted keys under it whose leaves
    /// changed, and returns its hash. Counts each node hash in `hashed`.
    fn rehash(&mut self, depth: usize, prefix: B256, changed: &[B256], hashed: &mut usize) -> B256 {
        let hash = match self.subtree(depth, prefix) {
            Subtree::Node => {
                let (left, right) = changed.split_at(changed.partition_point(|key| !bit(key, depth)));
                let left = self.rehash_child(depth + 1, prefix, left, hashed);
                let right = self.rehash_child(depth + 1, with_bit(prefix, depth), right, hashed);
                let hash = node_hash(left, right);
                *hashed += 1;
                self.nodes.insert((depth, prefix), hash);
                return hash;
            }
            Subtree::Empty => EMPTY,
            Subtree::Leaf(leaf) => leaf,
        };
        // Left with one leaf or none, the subtree has no node. If it had one, removals emptied it,
        // and may have left nodes below it on their paths too.
        if self.nodes.remove(&(depth, prefix)).is_some() {
            let (left, right) = changed.split_at(changed.partition_point(|key| !bit(key, depth)));
            self.rehash_child(depth + 1, prefix, left, hashed);
            self.rehash_child(depth + 1, with_bit(prefix, depth), right, hashed);
        }
        hash
    }

    /// The hash of a child subtree: rehashed when it holds changed leaves, read otherwise.
    fn rehash_child(&mut self, depth: usize, prefix: B256, changed: &[B256], hashed: &mut usize) -> B256 {
        if changed.is_empty() { self.hash_of(depth, prefix) } else { self.rehash(depth, prefix, changed, hashed) }
    }

    /// The hash of the subtree at `depth` under `prefix`.
    fn hash_of(&self, depth: usize, prefix: B256) -> B256 {
        match self.subtree(depth, prefix) {
            Subtree::Empty => EMPTY,
            Subtree::Leaf(leaf) => leaf,
            // `rehash` stores the node of every subtree that reaches two leaves.
            Subtree::Node => *self.nodes.get(&(depth, prefix)).expect("a subtree of two or more leaves has a node"),
        }
    }

    /// What the subtree at `depth` under `prefix` holds.
    fn subtree(&self, depth: usize, prefix: B256) -> Subtree {
        let mut leaves = self.leaves.range(prefix..=last_under(prefix, depth)).map(|(_, leaf)| *leaf);
        match (leaves.next(), leaves.next()) {
            (None, _) => Subtree::Empty,
            (Some(leaf), None) => Subtree::Leaf(leaf),
            (Some(_), Some(_)) => Subtree::Node,
        }
    }
}

/// The hash of the leaf at `key` with `value_hash`.
pub fn leaf_hash(key: B256, value_hash: B256) -> B256 {
    keccak(&[&[LEAF_PREFIX], key.as_slice(), value_hash.as_slice()])
}

/// The hash of a node over its two children.
pub fn node_hash(left: B256, right: B256) -> B256 {
    keccak(&[&[NODE_PREFIX], left.as_slice(), right.as_slice()])
}

/// Whether `siblings`, from the root down, lead from the leaf at `key` with `value_hash` to `root`.
pub fn verify(root: B256, key: B256, value_hash: B256, siblings: &[B256]) -> bool {
    if siblings.len() > 256 {
        return false;
    }
    let top = siblings.iter().enumerate().rev().fold(leaf_hash(key, value_hash), |hash, (depth, sibling)| {
        if bit(&key, depth) { node_hash(*sibling, hash) } else { node_hash(hash, *sibling) }
    });
    top == root
}

/// keccak-256 over the concatenation of `parts`.
pub(crate) fn keccak(parts: &[&[u8]]) -> B256 {
    let mut hasher = Keccak256::new();
    parts.iter().for_each(|part| hasher.update(part));
    B256::from(<[u8; 32]>::from(hasher.finalize()))
}

/// Bit `depth` of `key`, counting from the most significant: whether its path turns right there.
fn bit(key: &B256, depth: usize) -> bool {
    key[depth / 8] & (0x80 >> (depth % 8)) != 0
}

/// `key` with every bit from `depth` on cleared: the prefix of its subtree at `depth`.
fn prefix(key: &B256, depth: usize) -> B256 {
    let mut out = *key;
    for (i, byte) in out.iter_mut().enumerate() {
        *byte &= !low_bits(depth, i);
    }
    out
}

/// `prefix` with every bit from `depth` on set: the last key under it at `depth`.
fn last_under(prefix: B256, depth: usize) -> B256 {
    let mut out = prefix;
    for (i, byte) in out.iter_mut().enumerate() {
        *byte |= low_bits(depth, i);
    }
    out
}

/// `prefix` with bit `depth` set: its right child's prefix.
fn with_bit(prefix: B256, depth: usize) -> B256 {
    let mut out = prefix;
    out[depth / 8] |= 0x80 >> (depth % 8);
    out
}

/// The bits of byte `i` at or past bit `depth` of a key.
fn low_bits(depth: usize, i: usize) -> u8 {
    let kept = depth.saturating_sub(i * 8).min(8);
    0xffu8.checked_shr(kept as u32).unwrap_or(0)
}

#[cfg(test)]
mod tests;
