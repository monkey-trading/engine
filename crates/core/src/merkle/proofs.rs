//! Account, entitlement, distribution and withdrawal-approval proofs against the last state root an
//! engine computed inline.

use crate::engine::Engine;
use crate::merkle::state::Tree;
use crate::merkle::tree::{StateTree, Version};
use crate::merkle::{AccountProof, DistributionProof, EntitlementProof, WithdrawalApprovalProof};
use crate::types::AccountId;

impl Engine {
    /// The engine's tree and the version of its last root, or `None` before the first root under
    /// `V9`, or with the tree detached.
    pub(crate) fn committed(&self) -> Option<(&StateTree, &Version)> {
        let Tree::Inline(tree) = &self.tree else { return None };
        Some((tree.as_ref(), tree.newest()?))
    }

    /// The proof of `account`'s leaf, of each of its position leaves, and of the leaf of each
    /// market it holds a position in, against the last state root computed. `None` before the
    /// first root under `V9`, or for an account that root does not hold.
    pub fn account_proof(&self, account: AccountId) -> Option<AccountProof> {
        let (tree, version) = self.committed()?;
        tree.account_proof(version, account)
    }

    /// The proof of `account`'s cumulative MONKEY entitlement against the last state root computed.
    /// `None` before the first root under `V9`, or for an account that root holds no entitlement for.
    pub fn entitlement_proof(&self, account: AccountId) -> Option<EntitlementProof> {
        let (tree, version) = self.committed()?;
        tree.entitlement_proof(version, account)
    }

    /// The proof of `account`'s cumulative USDC distribution against the last state root computed,
    /// as the vault's `claimDistribution` takes it. `None` before the first root under `V9`, or for an
    /// account that root holds no distribution for.
    pub fn distribution_proof(&self, account: AccountId) -> Option<DistributionProof> {
        let (tree, version) = self.committed()?;
        tree.distribution_proof(version, account)
    }

    /// Proves the exact immutable payout against the last computed root, or `None` until included.
    pub fn approval_proof(&self, account: AccountId, nonce: u64) -> Option<WithdrawalApprovalProof> {
        let (tree, version) = self.committed()?;
        tree.approval_proof(version, account, nonce)
    }
}
