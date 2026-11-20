//! The deposit scan's quorum reads: a window's deposits with the hashes of its last block and of the
//! boundary below it, as one answer, and the vault's count and record of a deposit in a block.

use futures_util::future::join_all;

use super::Quorum;
use crate::custody::evm::{BlockRef, RangedDeposits, VaultReader, VaultRecord};

impl<R: VaultReader + Sync> Quorum<R> {
    /// Identical complete log sets with the hash of the range's last block, read before them, and of
    /// `boundary`, read after them, each provider's reads voted together as one answer, so a provider
    /// that answers them from different branches cannot join a majority; a lagging absence cannot vote.
    /// Error cases:
    /// - no quorum, or a provider cannot return the range's last block or the boundary
    pub(super) async fn voted_window(
        &self,
        from: u64,
        to: u64,
        boundary: Option<u64>,
    ) -> anyhow::Result<RangedDeposits> {
        let answers = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            r.ranged_deposits(from, to, boundary).await
        }))
        .await;
        self.vote("deposits", answers).map(|v| v.0)
    }

    /// Identical counts at the same canonical block hash.
    /// Error cases:
    /// - no quorum
    pub(super) async fn voted_count(&self, at: BlockRef) -> anyhow::Result<u64> {
        let answers = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            r.deposit_count(at).await
        }))
        .await;
        self.vote("deposit_count", answers).map(|v| v.0)
    }

    /// The same record of deposit `id` at the same canonical block hash.
    /// Error cases:
    /// - no quorum
    pub(super) async fn voted_record(&self, at: BlockRef, id: u64) -> anyhow::Result<VaultRecord> {
        let answers = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            r.deposit_record(at, id).await
        }))
        .await;
        self.vote("deposit_record", answers).map(|v| v.0)
    }
}
