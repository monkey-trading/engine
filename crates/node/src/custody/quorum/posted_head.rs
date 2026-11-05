//! The LogHeads contract's posted head, read through the quorum at a block a strict majority agrees on.

use alloy::primitives::Address;
use anyhow::{bail, ensure};
use futures_util::future::join_all;

use super::{Quorum, last_header, majority};
use crate::custody::evm::{BlockRef, PostedHead, VaultReader};

impl<R: VaultReader + Sync> Quorum<R> {
    /// The LogHeads contract's head at `contract`, read at the highest block a strict majority has
    /// reached and agrees on, block and head both. Providers polled together sit a block or more
    /// apart on a fast chain, so their tips rarely match: from the highest height a strict majority
    /// has reached down to the lowest tip, each provider at or past the height answers its hash there
    /// (one past it reads its header), then each reads the head at the agreed block; the first height
    /// where a strict majority agrees on both wins. A faulty provider cannot deny the read while the
    /// honest ones agree at some height, and a lagging minority cannot vote. Candidates vote silently;
    /// only the deciding votes, or the last candidate's when none wins, raise or resolve the alerts.
    /// Returns the agreed block with the head, so a caller reads more at the same block.
    /// Error cases:
    /// - fewer than a strict majority answer, or no strict majority agrees on the block and the head
    ///   at any candidate height
    pub(crate) async fn posted_head(&self, contract: Address) -> anyhow::Result<(PostedHead, BlockRef)> {
        let tips = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            r.latest_block().await
        }))
        .await;
        let mut heights: Vec<u64> = tips.iter().filter_map(|a| a.as_ref().ok().map(|(number, _)| *number)).collect();
        heights.sort_unstable_by(|a, b| b.cmp(a));
        let mut candidates: Vec<u64> = heights.into_iter().skip(self.required - 1).collect();
        candidates.dedup();
        let (mut last_blocks, mut last_heads) = (None, None);
        for common in candidates {
            let blocks = join_all(self.providers.iter().zip(&tips).map(|((_, reader), tip)| async move {
                let &(number, hash) = tip.as_ref().map_err(|cause| anyhow::anyhow!("{cause:#}"))?;
                ensure!(number >= common, "provider has not reached the common latest block");
                if number == common {
                    return Ok((number, hash));
                }
                let header = last_header(reader.headers(common, common).await?, common)?;
                Ok((common, header.hash))
            }))
            .await;
            let Some(&(number, hash)) = majority(&blocks, self.required) else {
                last_blocks = Some(blocks);
                continue;
            };
            let at = BlockRef { number, hash };
            let heads = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
                self.qualified(i)?;
                r.log_head(contract, at).await
            }))
            .await;
            if majority(&heads, self.required).is_some() {
                self.vote("latest_block", blocks)?;
                return self.vote("log_head", heads).map(|v| (v.0, at));
            }
            last_heads = Some((heads, at));
        }
        if let Some((heads, at)) = last_heads {
            return self.vote("log_head", heads).map(|v| (v.0, at));
        }
        // No height won a strict majority on the block: the vote over the last candidate's answers
        // (or the tips, with too few to pick a height) raises the alert.
        self.vote("latest_block", last_blocks.unwrap_or(tips))?;
        bail!("custody RPC quorum lost for latest_block: no candidate height wins")
    }
}
