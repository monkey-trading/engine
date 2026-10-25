//! Proofs of an account's balance and positions, of the marks its positions are valued at, and of
//! its MONKEY entitlement, against a state root. Checking one needs only this module, not a node.
//! Each position and each market is a leaf of its own, so a verifier can check them in batches; the
//! account leaf's `open_positions` says how many position leaves make the whole account.

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::account::Position;
use crate::emissions::TokenUnits;
use crate::merkle::{
    AccountLeaf, MarketLeaf, account_key, entitlement_hash, entitlement_key, market_key, position_hash, position_key,
    verify,
};
use crate::types::{AccountId, MarketId};

/// An account's leaf with its proof, the leaf and proof of each of its positions, and those of
/// each market it holds a position in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountProof {
    /// The seq of the last command applied when the root was computed.
    pub seq: u64,
    /// The root the proof was taken against.
    pub root: B256,
    pub account: AccountId,
    pub leaf: AccountLeaf,
    /// Sibling hashes from the root down to the account's leaf.
    pub siblings: Vec<B256>,
    /// One per open position, in market order.
    pub positions: Vec<PositionProof>,
    /// The market of each position, in the same order.
    pub markets: Vec<MarketProof>,
}

/// One of an account's positions with its proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PositionProof {
    pub market: MarketId,
    pub position: Position,
    /// Sibling hashes from the root down to the position's leaf.
    pub siblings: Vec<B256>,
}

/// An account's cumulative MONKEY entitlement with its proof, as the vault's claim takes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntitlementProof {
    /// The seq of the last command applied when the root was computed.
    pub seq: u64,
    /// The root the proof was taken against.
    pub root: B256,
    pub account: AccountId,
    /// The account's cumulative entitlement in base units.
    pub entitled: TokenUnits,
    /// Sibling hashes from the root down to the entitlement's leaf.
    pub siblings: Vec<B256>,
}

/// A market's leaf with its proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketProof {
    pub market: MarketId,
    pub leaf: MarketLeaf,
    /// Sibling hashes from the root down to the market's leaf.
    pub siblings: Vec<B256>,
}

impl AccountProof {
    /// Whether the account's leaf, all `open_positions` of its positions in distinct markets, and
    /// the market of each, prove against `root`. Checks `root`, which the caller takes from a
    /// signed checkpoint, not the root the proof names.
    pub fn verify(&self, root: B256) -> bool {
        let markets = || self.positions.iter().map(|position| position.market);
        // Strictly increasing markets are distinct, so the count covers every position leaf.
        usize::try_from(self.leaf.open_positions).is_ok_and(|count| count == self.positions.len())
            && markets().zip(markets().skip(1)).all(|(a, b)| a < b)
            && markets().eq(self.markets.iter().map(|market| market.market))
            && verify(root, account_key(self.account), self.leaf.value_hash(), &self.siblings)
            && self.positions.iter().all(|position| position.verify(root, self.account))
            && self.markets.iter().all(|market| market.verify(root))
    }
}

impl PositionProof {
    /// Whether this is `account`'s position leaf under `root`.
    pub fn verify(&self, root: B256, account: AccountId) -> bool {
        verify(root, position_key(account, self.market), position_hash(&self.position), &self.siblings)
    }
}

impl EntitlementProof {
    /// Whether the account's entitlement leaf proves against `root`, which the caller takes from a
    /// signed checkpoint or the vault, not the root the proof names.
    pub fn verify(&self, root: B256) -> bool {
        verify(root, entitlement_key(self.account), entitlement_hash(self.entitled), &self.siblings)
    }
}

impl MarketProof {
    /// Whether the market's leaf proves against `root`.
    pub fn verify(&self, root: B256) -> bool {
        verify(root, market_key(self.market), self.leaf.value_hash(), &self.siblings)
    }
}
