//! The reversed-deposit leaves, from `V83`: kind 36 holds an account's deposits whose principal a
//! reversal took and what the house has written off against it since (`deposits::write_offs`). The value
//! hashes its JSON encoding, and the account leaf the vault's exit hashes is unchanged.

use std::collections::BTreeSet;

use alloy_primitives::B256;

use crate::engine::Engine;
use crate::merkle::{json_hash, keccak};
use crate::types::AccountId;

const REVERSED_DEPOSITS: u8 = 36;

/// The key of `account`'s reversed-deposit leaf.
pub fn reversed_deposits_key(account: AccountId) -> B256 {
    keccak(&[&[REVERSED_DEPOSITS], &account.0])
}

impl Engine {
    /// The reversed-deposit leaves of `accounts`, those changed since the last root, by key, `None` for
    /// one removed.
    pub(super) fn reversed_deposit_leaves(&self, accounts: &BTreeSet<AccountId>) -> Vec<(B256, Option<B256>)> {
        accounts.iter().map(|&id| (reversed_deposits_key(id), self.reversed_deposits.get(&id).map(json_hash))).collect()
    }
}
