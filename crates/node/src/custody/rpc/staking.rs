//! The staking mirror's log reads: one read over a window's block range, and reads pinned to the hash of
//! each block the range read returned an event in.

use alloy::primitives::Address;
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use anyhow::ensure;
use futures_util::{StreamExt, TryStreamExt, stream};

use crate::custody::evm::{BlockRef, StakingEvent};
use crate::custody::rpc::{Staked, UnstakeCompleted, VaultRpc};

impl VaultRpc {
    /// The log filter of every event the staking mirror reads, by contract and event signature, with no
    /// block range; `None` without a staking contract.
    pub(super) fn mirror_filter(&self) -> Option<Filter> {
        let staking = self.staking?;
        let addresses: Vec<Address> =
            [Some(staking), self.registry, self.schedule, self.vote, self.fault_proof].into_iter().flatten().collect();
        let mut signatures = vec![Staked::SIGNATURE_HASH, UnstakeCompleted::SIGNATURE_HASH];
        if self.registry.is_some() {
            signatures.extend(crate::custody::registry::SIGNATURES);
        }
        if self.schedule.is_some() {
            signatures.extend(crate::custody::registry::SCHEDULE_SIGNATURES);
        }
        if self.vote.is_some() {
            signatures.extend(crate::custody::registry::VOTE_SIGNATURES);
        }
        if self.fault_proof.is_some() {
            signatures.extend(crate::custody::registry::FAULT_SIGNATURES);
        }
        Some(Filter::new().address(addresses).event_signature(signatures))
    }

    /// Both mirrorable staking events from the configured contract, the operator registry's
    /// rotation switches and elections, its schedule's rotation sets and turns, the vote contract's
    /// passed proposals, and the fault proof contract's proven faults, over blocks `from..=to` in one
    /// log read, sorted by block and log index. Without a staking contract the answer is empty.
    /// Error cases:
    /// - the range is empty, or the request fails or times out
    /// - a log is removed, lacks its position, does not decode as a staking or registry event, or
    ///   carries an amount past `u128`, or a returned log is outside the range
    pub(super) async fn ranged_staking_events(&self, from: u64, to: u64) -> anyhow::Result<Vec<StakingEvent>> {
        ensure!(from <= to, "staking range is empty");
        let Some(filter) = self.mirror_filter() else { return Ok(Vec::new()) };
        let filter = filter.from_block(from).to_block(to);
        let logs = self.deadline("eth_getLogs", self.client.request::<_, Vec<Log>>("eth_getLogs", (filter,))).await?;
        let mut events = Vec::with_capacity(logs.len());
        for log in &logs {
            let event = self.decode_staking(log)?;
            ensure!((from..=to).contains(&event.block), "staking log is outside the requested range");
            events.push(event);
        }
        events.sort_by_key(|event| (event.block, event.log_index));
        Ok(events)
    }

    /// The same events as `staking_events`, read at each block by its hash, so a backend that does not
    /// hold that block refuses rather than answering from another fork.
    /// Error cases:
    /// - a request fails or times out, or a backend does not hold a block
    /// - a log is removed, lacks its position, or does not decode, or differs from its requested block
    pub(super) async fn hash_pinned_staking_events(&self, blocks: &[BlockRef]) -> anyhow::Result<Vec<StakingEvent>> {
        let Some(filter) = self.mirror_filter() else { return Ok(Vec::new()) };
        let batches = stream::iter(blocks.iter().copied().map(|block| {
            let filter = filter.clone().at_block_hash(block.hash);
            async move {
                let logs =
                    self.deadline("eth_getLogs", self.client.request::<_, Vec<Log>>("eth_getLogs", (filter,))).await?;
                let mut events = Vec::new();
                for log in &logs {
                    let event = self.decode_staking(log)?;
                    ensure!(
                        event.block == block.number && event.block_hash == block.hash,
                        "staking log differs from its requested block"
                    );
                    events.push(event);
                }
                Ok::<_, anyhow::Error>(events)
            }
        }))
        .buffered(8)
        .try_collect::<Vec<_>>()
        .await?;
        let mut events: Vec<_> = batches.into_iter().flatten().collect();
        events.sort_by_key(|event| (event.block, event.log_index));
        Ok(events)
    }
}
