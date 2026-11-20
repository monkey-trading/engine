//! What the latest assignment read costs a handover's switch (#826): the holder reads it after its
//! stop (`Predecessor::revalidate`) and the successor before it seals the keys (`Bound::recheck`),
//! so the switch window holds two such reads, each `ROUND_TRIPS` round trips to the custody
//! providers, one after another, and one more while a provider's tip is past the agreed block. A
//! deployment's switch budget, from which the take window's minimum is set, covers the daily
//! rotation scenario's measured switch, whose members read no latest assignment, plus twice that
//! many round trips at its providers' latency.

use super::*;

use std::time::Duration;

use crate::handover::assigned::tests::{LOG_HEADS, Scripted, TIME, script_turn};
use crate::handover::assigned::{Assignments, LatestAssignments};

/// The round trips to the providers one latest assignment read makes, one after another, each
/// provider's requests as `VaultRpc` makes them: the identity (the chain id, then the vault's token),
/// the tips, the posted head (the signer epoch, then the head) and the agreed header, then the vault's
/// registry, the registry's schedule, the rotation, turn and signer epoch together, and the assignment
/// and the head's release together; one more, a header, while a provider's tip is past the agreed
/// block.
const ROUND_TRIPS: u32 = 10;

#[tokio::test(start_paused = true)]
async fn a_latest_assignment_read_takes_a_fixed_number_of_round_trips() {
    let latency = Duration::from_millis(80);
    let mut block = Scripted::scheduled();
    script_turn(&mut block, (1, 1), &[(B256::repeat_byte(1), 7)]);
    let block = Arc::new(block);
    for (ahead, round_trips) in [(0, ROUND_TRIPS), (1, ROUND_TRIPS + 1)] {
        let lagged = |i: u64| Lagged { block: block.clone(), latency, ahead: if i == 2 { ahead } else { 0 } };
        let providers = (0..3).map(|i| (format!("p{i}"), lagged(i))).collect();
        let chain = StandbyReader::over(providers, LOG_HEADS, CHAIN_ID);
        let read = LatestAssignments { chain, chain_id: CHAIN_ID, vault: VAULT, log_heads: LOG_HEADS };
        let started = tokio::time::Instant::now();
        let assigned = read.assigned().await.unwrap().expect("a turn names the operator");
        assert_eq!(assigned.binding.generation, 7);
        assert_eq!(started.elapsed(), latency * round_trips, "ahead {ahead}: restate the switch budget");
    }
}

// ── Helpers ──

/// A provider at `block`, or `ahead` blocks past it, answering each request after `latency`, with as
/// many requests one after another as `VaultRpc` makes for each read.
struct Lagged {
    block: Arc<Scripted>,
    latency: Duration,
    ahead: u64,
}

impl VaultReader for Lagged {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        tokio::time::sleep(self.latency * 2).await;
        Ok(VaultIdentity { chain_id: CHAIN_ID, token: Some(Address::repeat_byte(0xc0)) })
    }

    async fn finality(&self) -> anyhow::Result<Finality> {
        anyhow::bail!("a latest read reads no finality")
    }

    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        tokio::time::sleep(self.latency).await;
        let hash = AgreedBlock::hash(&&*self.block);
        Ok((from..=to).map(|number| BlockLink { number, hash, parent_hash: B256::ZERO, timestamp: TIME }).collect())
    }

    async fn deposits(&self, _from: u64, _to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        anyhow::bail!("a latest read reads no deposits")
    }

    async fn deposit_count(&self, _at: BlockRef) -> anyhow::Result<u64> {
        anyhow::bail!("a latest read reads no deposit count")
    }

    async fn staking_events(&self, _blocks: &[BlockRef]) -> anyhow::Result<Vec<StakingEvent>> {
        anyhow::bail!("a latest read reads no staking events")
    }

    async fn latest_block(&self) -> anyhow::Result<(u64, B256)> {
        tokio::time::sleep(self.latency).await;
        Ok((AgreedBlock::number(&&*self.block) + self.ahead, AgreedBlock::hash(&&*self.block)))
    }

    async fn log_head(&self, _contract: Address, _at: BlockRef) -> anyhow::Result<PostedHead> {
        tokio::time::sleep(self.latency * 2).await;
        Ok(PostedHead { epoch: 3, head: None })
    }

    async fn contract_call(&self, _at: BlockRef, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        tokio::time::sleep(self.latency).await;
        AgreedBlock::call(&&*self.block, "latency", to, data).await
    }
}
