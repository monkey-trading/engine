//! The staking mirror's quorum reads: a window's events over its block range, bound to the endpoint
//! the watcher proved, and events read at each block by its hash.

use anyhow::{Context, ensure};
use futures_util::future::join_all;

use super::{Quorum, last_header};
use crate::custody::evm::{BlockRef, StakingEvent, VaultReader};

impl<R: VaultReader + Sync> Quorum<R> {
    /// Identical staking event sets anchored to the range endpoint the watcher already proved.
    /// Error cases:
    /// - no quorum, or the winning endpoint differs from that proof, even for an empty response
    pub(super) async fn ranged_staking_events(&self, blocks: &[BlockRef]) -> anyhow::Result<Vec<StakingEvent>> {
        let to = blocks.last().context("staking range is empty")?;
        let answers = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            let header = last_header(r.headers(to.number, to.number).await?, to.number)?;
            let mut events = r.staking_events(blocks).await?;
            events.sort_by_key(|event| (event.block, event.log_index));
            Ok((header.hash, events))
        }))
        .await;
        let ((hash, events), _) = self.vote("staking_events", answers)?;
        ensure!(hash == to.hash, "staking event range endpoint differs from its proven canonical hash");
        Ok(events)
    }

    /// Identical staking event sets, each provider's read pinned to every block's hash.
    /// Error cases:
    /// - no quorum
    pub(super) async fn hash_pinned_staking_events(&self, blocks: &[BlockRef]) -> anyhow::Result<Vec<StakingEvent>> {
        let answers = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            let mut events = r.pinned_staking_events(blocks).await?;
            events.sort_by_key(|event| (event.block, event.log_index));
            Ok(events)
        }))
        .await;
        self.vote("pinned_staking_events", answers).map(|v| v.0)
    }
}
