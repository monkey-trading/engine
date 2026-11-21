//! A reader that answers the staking mirror from a fixed event list.

use crate::custody::chain::BlockLink;
use crate::custody::evm::{BlockRef, Finality, StakingEvent, VaultDeposit, VaultIdentity, VaultReader};
use crate::custody::tests::fakes::{CHAIN_ID, canonical_hash};

/// A reader answering `staking_events` and their reads by hash from a fixed event list, with canonical
/// headers.
pub(super) struct StubReader(pub(super) Vec<StakingEvent>);

impl VaultReader for StubReader {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        Ok(VaultIdentity { chain_id: CHAIN_ID, token: None })
    }

    async fn finality(&self) -> anyhow::Result<Finality> {
        Ok(Finality { head: 0, head_time: 0, tagged: None, head_support: None })
    }

    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        Ok((from..=to)
            .map(|number| BlockLink {
                number,
                hash: canonical_hash(number),
                parent_hash: canonical_hash(number.saturating_sub(1)),
                timestamp: number * 12,
            })
            .collect())
    }

    async fn deposits(&self, from: u64, to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        let _ = (from, to);
        Ok(Vec::new())
    }

    async fn deposit_count(&self, at: BlockRef) -> anyhow::Result<u64> {
        let _ = at;
        Ok(0)
    }

    async fn staking_events(&self, from: u64, to: BlockRef) -> anyhow::Result<Vec<StakingEvent>> {
        anyhow::ensure!(to.hash == canonical_hash(to.number), "staking endpoint is not canonical");
        Ok(self.0.iter().filter(|e| (from..=to.number).contains(&e.block)).cloned().collect())
    }

    async fn pinned_staking_events(&self, blocks: &[BlockRef]) -> anyhow::Result<Vec<StakingEvent>> {
        anyhow::ensure!(blocks.iter().all(|block| block.hash == canonical_hash(block.number)), "unknown block");
        Ok(self.0.iter().filter(|e| blocks.iter().any(|block| block.number == e.block)).cloned().collect())
    }
}
