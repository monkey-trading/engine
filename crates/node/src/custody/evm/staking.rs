//! Finalized staking catch-up, independent of deposit scan progress.

use anyhow::anyhow;
use tokio::sync::mpsc;
use tracing::info;

use crate::custody::credit::query;
use crate::custody::evm::{DepositWatcher, VaultReader};
use crate::custody::staking;
use crate::custody::stall::PollError;
use crate::sequencer::Request;

impl<V: VaultReader> DepositWatcher<V> {
    /// Mirrors the configured staking contract's events through the Ethereum-finalised boundary
    /// this poll proved (`release_at`), never the merely confirmed scan end: a staking mirror has
    /// no reversal, so a reorged event must never become a permanent engine balance. Catch-up
    /// runs in bounded chunks whose proofs are retained on the staking mirror's own slot, so it
    /// finishes across polls however deposits advance. Without a staking contract, a finalised
    /// boundary, or before `InitStaking` has applied, nothing is read or sequenced and the cursor
    /// holds (see `staking::mirror`).
    /// Error cases:
    /// - stalled: a staking read fails, an event is on an unproven block, or the engine refuses a mirror
    /// - fatal: the sequencer has stopped, or the staking cursor cannot be stored
    pub(super) async fn mirror_staking(
        &mut self,
        tx: &mpsc::Sender<Request>,
        release_at: Option<u64>,
    ) -> Result<(), PollError> {
        let Some(mut to) = release_at else { return Ok(()) };
        if self.staking.is_none() {
            return Ok(());
        };
        let configured_start = self.start_block;
        let Some((from, anchor)) = query(tx, move |engine| {
            engine.staking().map(|staking| {
                let from = staking.completed_through.map_or_else(
                    || staking.mirrored_through.unwrap_or(configured_start).max(configured_start),
                    |checkpoint| checkpoint.block + 1,
                );
                (from, staking.completed_through)
            })
        })
        .await
        .map_err(PollError::Fatal)?
        else {
            return Ok(());
        };
        if let Some(anchor) = anchor {
            let links = self.headers(anchor.block, anchor.block).await?;
            if links.len() != 1 || links[0].number != anchor.block || links[0].hash != anchor.block_hash {
                return Err(PollError::inconsistent(anyhow!("finalized staking anchor changed")));
            }
            self.chain.insert(anchor.block, anchor.block_hash);
        }
        if from > to {
            return Ok(());
        }
        // Bound the chunk so its proof completes within header batches and the retained run
        // survives deposit proofs; the cursor advances per completed chunk until it reaches the
        // release tag, so catch-up reports progress instead of restarting against a moving end.
        to = to.min(from.saturating_add(self.max_block_range.saturating_sub(1)));
        let Some(hashes) = self.staking_window(from, to).await? else {
            info!(from, to, "staking mirror is proving a bounded catch-up window");
            return Ok(());
        };
        let (cursor, path) = self.staking.as_mut().expect("staking cursor checked above");
        staking::mirror(&self.vault, tx, cursor, path, from, to, &hashes).await
    }
}
