//! The award leaves, from `V89`: kind 37 holds an award a vote approved and what the epoch closes have paid
//! of it (`awards`), keyed by its id. The value hashes its JSON encoding.

use std::collections::BTreeSet;

use alloy_primitives::B256;

use crate::engine::Engine;
use crate::merkle::{json_hash, keccak};

const AWARD: u8 = 37;

/// The key of award `id`'s leaf.
pub fn award_key(id: u64) -> B256 {
    keccak(&[&[AWARD], &id.to_be_bytes()])
}

impl Engine {
    /// The leaves of the awards `ids`, those changed since the last root, by key.
    pub(super) fn award_leaves(&self, ids: &BTreeSet<u64>) -> Vec<(B256, Option<B256>)> {
        ids.iter().map(|&id| (award_key(id), self.awards.get(&id).map(json_hash))).collect()
    }
}
