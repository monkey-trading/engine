//! The volume leaves, from `V59`: kind 28 holds an account's volume book (`address`), its counted
//! volume by day and the notional waiting to count (`volume_tier`). The value hashes its JSON
//! encoding, and the account leaf the vault's exit hashes is unchanged.

use std::collections::BTreeSet;

use alloy_primitives::B256;

use crate::engine::Engine;
use crate::merkle::{json_hash, keccak};
use crate::types::AccountId;

const VOLUME: u8 = 28;

/// The key of `account`'s volume leaf.
pub fn volume_key(account: AccountId) -> B256 {
    keccak(&[&[VOLUME], &account.0])
}

impl Engine {
    /// The volume leaves of `accounts`, those changed since the last root, by key, `None` for one
    /// removed.
    pub(super) fn volume_leaves(&self, accounts: BTreeSet<AccountId>) -> Vec<(B256, Option<B256>)> {
        accounts.into_iter().map(|id| (volume_key(id), self.volumes.get(&id).map(json_hash))).collect()
    }
}
