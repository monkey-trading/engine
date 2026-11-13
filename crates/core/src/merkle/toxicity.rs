//! The toxicity score's leaves, from `V54`: kind 25 holds an account's score (`address`), and kind 26
//! a market's markout spread history (`uint32`). Each value hashes its JSON encoding.

use std::collections::BTreeSet;

use alloy_primitives::B256;

use crate::engine::Engine;
use crate::merkle::{json_hash, keccak};
use crate::types::{AccountId, MarketId};

const TOXICITY_SCORE: u8 = 25;
const MARKOUT_SPREAD: u8 = 26;

/// The key of `account`'s toxicity score leaf.
pub fn toxicity_score_key(account: AccountId) -> B256 {
    keccak(&[&[TOXICITY_SCORE], &account.0])
}

/// The key of `market`'s markout spread leaf.
pub fn markout_spread_key(market: MarketId) -> B256 {
    keccak(&[&[MARKOUT_SPREAD], &market.to_be_bytes()])
}

impl Engine {
    /// The leaves of the scores and markout spreads changed since the last root, by key, `None` for
    /// one removed.
    pub(super) fn toxicity_leaves(
        &self,
        scores: BTreeSet<AccountId>,
        spreads: BTreeSet<MarketId>,
    ) -> Vec<(B256, Option<B256>)> {
        let scores =
            scores.into_iter().map(|id| (toxicity_score_key(id), self.toxicity_scores.get(&id).map(json_hash)));
        let spreads =
            spreads.into_iter().map(|id| (markout_spread_key(id), self.markout_spreads.get(&id).map(json_hash)));
        scores.chain(spreads).collect()
    }
}
