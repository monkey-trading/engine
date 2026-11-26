//! The deposit record leaves, from `V91`: kind 38 holds a pending deposit's record and kind 39 a held deposit's,
//! keyed by its reference's UTF-8 bytes, each valued the keccak-256 of the record's JSON encoding. A receipt-backed
//! deposit's own leaf binds only what the vault's exit checks (`exit.rs`), so these bind the rest: its reference
//! and, for a held one, its payer as screened and the reason it is held.

use std::collections::BTreeSet;

use alloy_primitives::B256;

use crate::engine::Engine;
use crate::merkle::{json_hash, keccak};
use crate::protocol::ProtocolVersion;

const PENDING_RECORD: u8 = 38;
const HELD_RECORD: u8 = 39;

/// The key of the record leaf of the pending deposit at `reference`.
fn pending_record_key(reference: &str) -> B256 {
    keccak(&[&[PENDING_RECORD], reference.as_bytes()])
}

/// The key of the record leaf of the held deposit at `reference`.
fn held_record_key(reference: &str) -> B256 {
    keccak(&[&[HELD_RECORD], reference.as_bytes()])
}

impl Engine {
    /// The record leaves of the pending deposits at `pending` and the held ones at `holds`, those changed since the
    /// last root, by key: none before `V91`.
    pub(super) fn deposit_record_leaves(
        &self,
        pending: &BTreeSet<String>,
        holds: &BTreeSet<String>,
    ) -> Vec<(B256, Option<B256>)> {
        if self.protocol_version < ProtocolVersion::V91 {
            return Vec::new();
        }
        let pending = pending
            .iter()
            .map(|reference| (pending_record_key(reference), self.pending_deposits.get(reference).map(json_hash)));
        let held = holds
            .iter()
            .map(|reference| (held_record_key(reference), self.held_deposits.get(reference).map(json_hash)));
        pending.chain(held).collect()
    }

    /// Records every pending and held deposit as changed, so the root at `V91`'s activation commits each one's
    /// record leaf.
    pub(crate) fn touch_deposit_records(&mut self) {
        let pending: Vec<String> = self.pending_deposits.keys().cloned().collect();
        for reference in pending {
            self.pending_deposits.get_mut(&reference);
        }
        let holds: Vec<String> = self.held_deposits.keys().cloned().collect();
        for reference in holds {
            self.held_deposits.get_mut(&reference);
        }
    }
}
