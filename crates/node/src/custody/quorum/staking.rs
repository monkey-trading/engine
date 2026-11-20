//! The staking mirror's quorum reads: a window's events over its block range, bound to the hash of its
//! last block, and events read at each block by its hash.

use anyhow::ensure;
use futures_util::future::join_all;

use super::{Quorum, last_header};
use crate::custody::evm::{BlockRef, StakingEvent, VaultReader};

impl<R: VaultReader + Sync> Quorum<R> {
    /// Identical staking event sets over `from..=to.number`, each provider's read after its own header at
    /// the range's last block, anchored to `to`'s hash.
    /// Error cases:
    /// - no quorum, or the winning endpoint differs from `to`, even for an empty response
    pub(super) async fn ranged_staking_events(&self, from: u64, to: BlockRef) -> anyhow::Result<Vec<StakingEvent>> {
        let answers = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            let header = last_header(r.headers(to.number, to.number).await?, to.number)?;
            let mut events = r.staking_events(from, to).await?;
            events.sort_by_key(|event| (event.block, event.log_index));
            Ok((header.hash, events))
        }))
        .await;
        let ((hash, events), _) = self.vote("staking_events", answers)?;
        ensure!(hash == to.hash, "staking event range endpoint differs from its canonical hash");
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
