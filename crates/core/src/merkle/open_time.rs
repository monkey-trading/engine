//! The open-time leaves, from `V55`: kind 27 holds a position's open time (`holding_fee`), keyed by
//! its account (`address`) and market (`uint32`) as the position's own leaf is, with the value hash
//! `keccak256(abi.encode(int64 openedAt))`. The position leaf keeps `abi.encode(qty, entry)`, which
//! the vault's exit checks, so the open time sits beside it.

use std::collections::BTreeSet;

use alloy_primitives::B256;
use alloy_sol_types::SolValue;

use crate::engine::Engine;
use crate::merkle::keccak;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId, Millis};

const OPEN_TIME: u8 = 27;

/// The key of the open time of `account`'s position in `market`.
pub fn open_time_key(account: AccountId, market: MarketId) -> B256 {
    keccak(&[&[OPEN_TIME], &account.0, &market.to_be_bytes()])
}

/// The value hash of an open-time leaf.
pub fn open_time_hash(opened_at: Millis) -> B256 {
    keccak(&[&opened_at.abi_encode()])
}

impl Engine {
    /// From `V55`, the open-time leaves of `positions`, the positions changed since the last root, by
    /// key, `None` for one closed: whatever moves an open time records its position as changed.
    pub(super) fn open_time_leaves(&self, positions: &BTreeSet<(AccountId, MarketId)>) -> Vec<(B256, Option<B256>)> {
        if self.protocol_version < ProtocolVersion::V55 {
            return Vec::new();
        }
        positions
            .iter()
            .map(|&(account, market)| {
                let opened_at = self.accounts.get(&account).and_then(|acct| acct.opened_at.get(&market));
                (open_time_key(account, market), opened_at.map(|at| open_time_hash(*at)))
            })
            .collect()
    }
}
