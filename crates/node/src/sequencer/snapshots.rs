//! Checkpoint capture for paired recovery caches; public audit replay stays on the full log path.

mod paired_recovery;

use anyhow::Context;
use tracing::warn;

use crate::log::{Checkpoint, CheckpointDomain};
use crate::sequencer::Sequencer;
use crate::snapshot::{self, RecoveryContext, Snapshot};

/// A checkpoint whose capture waits, with its progress and recovery facts as they stood then.
pub(super) struct Deferred {
    checkpoint: Checkpoint,
    progress: snapshot::Progress,
    context: RecoveryContext,
}

impl Sequencer {
    /// Captures a paired cache at this checkpoint, then collects retired catalog versions. Catalog
    /// collection runs here, once per checkpoint whether or not a capture was taken, instead of on
    /// every flush. A failed log sync fails closed; anything else only warns.
    /// Error cases:
    /// - the authoritative log cannot be synced
    pub(super) fn store_snapshot(&mut self, checkpoint: Checkpoint) -> anyhow::Result<()> {
        self.capture_snapshot(checkpoint)?;
        self.maintain_catalog();
        Ok(())
    }

    /// Collects retired catalog versions, warning when the catalog cannot take the work.
    pub(super) fn maintain_catalog(&mut self) {
        if let Err(reason) = self.log.maintain_catalog() {
            warn!(alert = "log_catalog_unavailable", "catalog maintenance deferred: {reason:#}");
        }
    }

    /// Seals the catalog at this checkpoint and hands the capture to the history owner, which
    /// publishes the paired cache. A capture is skipped while the previous one is in flight.
    /// Error cases:
    /// - the authoritative log cannot be synced
    fn capture_snapshot(&mut self, checkpoint: Checkpoint) -> anyhow::Result<()> {
        self.uncaptured = None;
        if self.sealing.is_none() || self.progress.genesis.ensure_closed().is_err() {
            return Ok(());
        }
        let Some(base) = self.snapshot_base.clone() else {
            // A missing finalized witness cannot become a digest or zero accounting.
            return Ok(());
        };
        let context = self.recovery_context(base);
        self.capture(Deferred { checkpoint, progress: self.progress.clone(), context })
    }

    /// Captures a checkpoint with the recovery facts it had when it was written, which custody
    /// status may have moved on since; with a capture in flight, keeps it for shutdown instead.
    /// Error cases:
    /// - the authoritative log cannot be synced
    pub(super) fn capture(&mut self, deferred: Deferred) -> anyhow::Result<()> {
        let Some(endpoint) = self.sealing.clone() else { return Ok(()) };
        let Some(permit) = endpoint.try_permit() else {
            // Shutdown captures it after all if no later checkpoint follows.
            self.uncaptured = Some(deferred);
            return Ok(());
        };
        let Deferred { checkpoint, progress, context } = deferred;
        self.log.sync_unpublished()?;
        let prepared = (|| {
            let (catalog, catalog_pin) = self.log.seal_catalog_boundary_unpublished()?;
            let snapshot = self.snapshot_payload(checkpoint, progress, context, catalog.checkpoint.offset)?;
            endpoint.submit(crate::snapshot::sealing::Capture {
                snapshot,
                ts: self.last_ts,
                catalog_store: endpoint.catalog_store,
                catalog,
                catalog_pin,
                _permit: permit,
            })
        })();
        if let Err(reason) = prepared {
            endpoint
                .write_alert
                .failed(|| warn!(alert = "snapshot_write_failed", "checkpoint remains in log: {reason:#}"));
        }
        Ok(())
    }

    /// The recovery facts at the checkpoint just written: concrete outstanding work, accounting,
    /// deferred withdrawals, and the finalized base.
    fn recovery_context(&self, base: snapshot::RecoveryBase) -> RecoveryContext {
        RecoveryContext {
            commands: self.next_seq,
            command_prefix: self.command_prefix,
            accounting: self.accounting,
            logs_events: true,
            unsigned: Vec::new(),
            deferred: self.gate.as_ref().map_or_else(Vec::new, |gate| gate.pending.clone()),
            checkpoint_signers: self.snapshot_signers.clone(),
            base,
            work: self.snapshot_work.clone(),
        }
    }

    /// Captures the engine at this checkpoint, whose entry is at `offset`, with the progress and
    /// recovery facts it had when written. No command has been applied since.
    /// Error cases:
    /// - engine encoding or recovery context validation fails
    fn snapshot_payload(
        &self,
        checkpoint: Checkpoint,
        progress: snapshot::Progress,
        context: RecoveryContext,
        offset: crate::log::Offset,
    ) -> anyhow::Result<Snapshot> {
        let snapshot = Snapshot {
            schema: 2,
            chain_id: self.chain_id,
            vault: self.vault,
            checkpoint,
            offset,
            engine: serde_json::to_string(&self.engine).context("encoding snapshot engine")?,
            progress,
            context,
            pair: None,
        };
        snapshot
            .context
            .validate(&snapshot.checkpoint, CheckpointDomain { chain_id: self.chain_id, vault: self.vault })?;
        Ok(snapshot)
    }
}

#[cfg(test)]
mod context_tests;
#[cfg(test)]
mod custody_tests;
