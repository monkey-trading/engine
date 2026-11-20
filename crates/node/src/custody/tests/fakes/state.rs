//! The fake chain's state helpers: its hashes after reorgs, and the deposits and final block a test sets.

use alloy::primitives::B256;

use crate::custody::evm::VaultDeposit;
use crate::custody::tests::fakes::{ChainState, FakeChain, canonical_hash};

impl ChainState {
    /// The canonical hash at `number`, forks included.
    pub(super) fn hash_at(&self, number: u64) -> B256 {
        self.forks.get(&number).copied().unwrap_or_else(|| canonical_hash(number))
    }
}

impl FakeChain {
    pub(in crate::custody::tests) fn add(&self, deposit: VaultDeposit) {
        self.0.lock().unwrap().deposits.push(deposit);
    }

    pub(in crate::custody::tests) fn remove(&self, deposit: &VaultDeposit) {
        self.0.lock().unwrap().deposits.retain(|d| d != deposit);
    }

    pub(in crate::custody::tests) fn set_final(&self, block: Option<u64>) {
        self.0.lock().unwrap().final_block = block;
    }

    pub(in crate::custody::tests) fn reads(&self) -> Vec<(u64, u64)> {
        self.0.lock().unwrap().reads.clone()
    }
}
