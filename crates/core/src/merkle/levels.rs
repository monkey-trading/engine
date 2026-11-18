//! The levels leaves, from `V79`: kind 29 holds a position's take profit and stop loss (`levels`), keyed
//! by its account (`address`) and market (`uint32`) as the position's own leaf is, with the value hash
//! `keccak256(abi.encode(bool hasTakeProfit, int128 takeProfit, bool hasStopLoss, int128 stopLoss))`,
//! an absent level encoding as `false, 0`. A position without levels has no such leaf.

use std::collections::BTreeSet;

use alloy_primitives::B256;
use alloy_sol_types::SolValue;

use crate::engine::Engine;
use crate::levels::PositionLevels;
use crate::merkle::keccak;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId};

const LEVELS: u8 = 29;

/// The key of the levels of `account`'s position in `market`.
pub fn levels_key(account: AccountId, market: MarketId) -> B256 {
    keccak(&[&[LEVELS], &account.0, &market.to_be_bytes()])
}

/// The value hash of a levels leaf.
pub fn levels_hash(levels: &PositionLevels) -> B256 {
    // Every field is named, so a field added to `PositionLevels` fails to compile until the leaf holds it.
    let PositionLevels { take_profit, stop_loss } = levels;
    let encoded = (
        take_profit.is_some(),
        take_profit.unwrap_or_default().raw(),
        stop_loss.is_some(),
        stop_loss.unwrap_or_default().raw(),
    );
    keccak(&[&encoded.abi_encode()])
}

impl Engine {
    /// From `V79`, the levels leaves of `positions`, the positions changed since the last root, by key,
    /// with each position's levels, `None` for one without: whatever changes levels records their
    /// position as changed.
    #[allow(clippy::type_complexity)]
    pub(super) fn levels_leaves(
        &self,
        positions: &BTreeSet<(AccountId, MarketId)>,
    ) -> Vec<((AccountId, MarketId), Option<PositionLevels>)> {
        if self.protocol_version < ProtocolVersion::V79 {
            return Vec::new();
        }
        positions
            .iter()
            .map(|&(account, market)| {
                let levels = self.accounts.get(&account).and_then(|acct| acct.levels.get(&market)).copied();
                ((account, market), levels)
            })
            .collect()
    }
}
