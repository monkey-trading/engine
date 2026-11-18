//! Account, entitlement, and withdrawal-approval proofs against the last state root.

use crate::engine::Engine;
use crate::merkle::{
    AccountProof, EntitlementProof, LevelsProof, MarketProof, NoLevelsProof, PositionProof, WithdrawalApprovalProof,
    account_key, entitlement_key, levels_key, market_key, position_key, withdrawal_approval_key,
};
use crate::types::{AccountId, MarketId};

impl Engine {
    /// The proof of `account`'s leaf, of each of its position leaves, and of the leaf of each
    /// market it holds a position in, against the last state root computed. `None` before the
    /// first root under `V9`, or for an account that root does not hold.
    pub fn account_proof(&self, account: AccountId) -> Option<AccountProof> {
        let state = self.tree.as_ref()?;
        let leaf = state.accounts.get(&account)?.clone();
        let proof = |key| state.tree.proof(key).expect("every leaf kept for proofs is in the tree");
        let held = state.positions.range((account, MarketId::MIN)..=(account, MarketId::MAX));
        let (positions, markets): (Vec<PositionProof>, Vec<MarketProof>) = held
            .map(|((_, market), position)| {
                let siblings = proof(position_key(account, *market));
                // Positions are only ever opened in listed markets, and listings are never removed.
                let market_leaf = state.markets.get(market).expect("a position's market has a leaf").clone();
                (
                    PositionProof { market: *market, position: *position, siblings },
                    MarketProof { market: *market, leaf: market_leaf, siblings: proof(market_key(*market)) },
                )
            })
            .unzip();
        let siblings = proof(account_key(account));
        let mut levels = Vec::new();
        let mut no_levels = Vec::new();
        for position in &positions {
            let market = position.market;
            match state.levels.get(&(account, market)) {
                Some(held) => {
                    levels.push(LevelsProof { market, levels: *held, siblings: proof(levels_key(account, market)) })
                }
                None => {
                    let (siblings, other) =
                        state.tree.absence(levels_key(account, market)).expect("a position without levels has no leaf");
                    no_levels.push(NoLevelsProof { market, other, siblings });
                }
            }
        }
        Some(AccountProof {
            seq: state.seq,
            root: state.root,
            account,
            leaf,
            siblings,
            positions,
            markets,
            levels,
            no_levels,
        })
    }

    /// The proof of `account`'s cumulative MONKEY entitlement against the last state root computed.
    /// `None` before the first root under `V9`, or for an account that root holds no entitlement for.
    pub fn entitlement_proof(&self, account: AccountId) -> Option<EntitlementProof> {
        let state = self.tree.as_ref()?;
        let entitled = *state.entitlements.get(&account)?;
        let siblings =
            state.tree.proof(entitlement_key(account)).expect("every entitlement kept for proofs is in the tree");
        Some(EntitlementProof { seq: state.seq, root: state.root, account, entitled, siblings })
    }

    /// Proves the exact immutable payout against the last computed root, or `None` until included.
    pub fn approval_proof(&self, account: AccountId, nonce: u64) -> Option<WithdrawalApprovalProof> {
        let state = self.tree.as_ref()?;
        let approval = *self.withdrawal_approval(account, nonce)?;
        let siblings = state.tree.proof(withdrawal_approval_key(account, nonce))?;
        Some(WithdrawalApprovalProof { seq: state.seq, root: state.root, approval, siblings })
    }
}
