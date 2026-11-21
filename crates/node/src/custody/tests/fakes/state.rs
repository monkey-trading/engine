//! The fake chain's state helpers: its hashes after reorgs, and the deposits and final block a test sets.

use alloy::primitives::{Address, B256};

use crate::custody::evm::{Beneficiary, BlockRef, VaultDeposit, VaultRecord};
use crate::custody::tests::fakes::{ChainState, FakeChain, canonical_hash};

impl ChainState {
    /// The canonical hash at `number`, forks included.
    pub(super) fn hash_at(&self, number: u64) -> B256 {
        self.forks.get(&number).copied().unwrap_or_else(|| canonical_hash(number))
    }

    /// The vault's record of deposit `id` in canonical block `at`, as `depositRecords` and
    /// `depositAccounts` answer: the deposit with that id in or below the block.
    /// Error cases:
    /// - `at` is not canonical, or no deposit with that id is in or below it
    pub(super) fn record(&self, at: BlockRef, id: u64) -> anyhow::Result<VaultRecord> {
        anyhow::ensure!(self.hash_at(at.number) == at.hash, "block {} is not canonical", at.hash);
        let deposit = self.deposits.iter().find(|d| d.id == id && d.block <= at.number);
        let deposit = deposit.ok_or_else(|| anyhow::anyhow!("no deposit {id} by block {}", at.number))?;
        let account = match deposit.beneficiary {
            Beneficiary::Trader(account) => Address::from(account.0),
            Beneficiary::House => Address::ZERO,
        };
        let payer = deposit.payer.map_or(Address::ZERO, |payer| Address::from(payer.0));
        Ok(VaultRecord { payer, units: deposit.units, account })
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
