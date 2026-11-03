//! A signed checkpoint reaches the poster only once it is durable in the log.

use std::collections::BTreeSet;

use anyhow::Context;
use engine_core::types::AccountId;
use tracing::warn;

use crate::checkpoints::Work;
use crate::log::Checkpoint;
use crate::sequencer::Sequencer;

/// What a checkpoint does once the batch it is in is durable: its work goes to the poster, and the
/// approvals it covers become visible to the poster's nonce-use queries. Neither may happen before
/// the checkpoint is in the log, as a checkpoint is public the moment the poster has it.
pub(in crate::sequencer) struct After {
    work: Work,
    payouts: BTreeSet<(AccountId, u64)>,
}

impl Sequencer {
    /// Makes a signed checkpoint known to the poster: now, once the log is synced, or with a
    /// sequencer that syncs on its own thread, when the batch holding it is durable (`run_after`).
    /// Its work is registered either way, so the next attempt sees it on its way.
    /// Error cases:
    /// - the log cannot be synced, the registries are poisoned, or the worker is unavailable
    pub(super) fn publish_checkpoint(&mut self, checkpoint: &Checkpoint) -> anyhow::Result<()> {
        if self.pipeline.is_none() {
            self.log.sync_unpublished()?;
            self.register_payouts()?;
            return self.queue_checkpoint(checkpoint.clone());
        }
        let work = Work::capture(checkpoint.clone(), self.accounting, &self.engine);
        let gate = self.gate.as_mut().context("checkpoint gate is absent")?;
        if gate.sink.is_closed() {
            warn!(alert = "checkpoint_post_failed", "checkpoint worker stopped; durable checkpoint awaits restart");
            return Ok(());
        }
        if let Some(registry) = &gate.registry {
            registry
                .lock()
                .map_err(|_| anyhow::anyhow!("checkpoint work registry is poisoned"))?
                .insert(work.checkpoint.seq, work.clone());
        }
        gate.deferred_posts += 1;
        let payouts = std::mem::take(&mut gate.unregistered);
        self.after_sync.push(After { work, payouts });
        Ok(())
    }

    /// Does what checkpoints waited for until their batch was durable.
    /// Error cases:
    /// - the approval registry is poisoned or conflicts, or the worker is unavailable or its queue full
    pub(in crate::sequencer) fn run_after(&mut self, after: Vec<After>) -> anyhow::Result<()> {
        for step in after {
            let After { work, payouts } = step;
            self.register_payout_keys(payouts)?;
            let gate = self.gate.as_mut().context("checkpoint gate is absent")?;
            gate.deferred_posts = gate.deferred_posts.saturating_sub(1);
            if gate.sink.is_closed() {
                warn!(alert = "checkpoint_post_failed", "checkpoint worker stopped; durable checkpoint awaits restart");
                continue;
            }
            let seq = work.checkpoint.seq;
            gate.sink
                .try_send(work)
                .map_err(|_| anyhow::anyhow!("checkpoint {seq}: checkpoint worker unavailable or queue full"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
