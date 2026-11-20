//! Checkpoint capture for paired recovery caches; public audit replay stays on the full log path.
//! A capture is due every `[snapshots] interval` of sequenced time, and its engine is encoded by the
//! stats worker's copy, so the sequencer's part is the log sync, the catalog seal and the recovery
//! facts it already holds.

mod paired_recovery;

use anyhow::Context;
use engine_core::types::Millis;
use tracing::warn;

use crate::log::{Checkpoint, CheckpointDomain};
use crate::sequencer::Sequencer;
use crate::snapshot::sealing::Encoding;
use crate::snapshot::{self, RecoveryContext, Snapshot};
use crate::stats::background::Job;

/// A checkpoint whose capture waits, with its progress and recovery facts as they stood then.
pub(super) struct Deferred {
    checkpoint: Checkpoint,
    progress: snapshot::Progress,
    context: RecoveryContext,
}

/// The capture cadence.
#[derive(Default)]
pub(super) struct Captures {
    /// The newest checkpoint, with its recovery facts as they stood then, when it was not captured:
    /// none was due, or another capture was in flight. Shutdown captures it if no checkpoint follows.
    pub(super) uncaptured: Option<Deferred>,
    /// The stamp from which a checkpoint's capture is due; `None` until the first capture.
    due: Option<Millis>,
    /// How long each capture held the sequencer's thread, from its permit to its handover, whether
    /// the history owner took it or not. For the recovery scenario.
    #[cfg(test)]
    pub(super) held: Vec<std::time::Duration>,
}

impl Sequencer {
    /// Captures a paired cache at this checkpoint when one is due, then collects retired catalog
    /// versions. Catalog collection runs here, once per checkpoint whether or not a capture was
    /// taken, instead of on every flush. A failed log sync fails closed; anything else only warns.
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

    /// Takes no cache at shutdown of a log a handover has sealed and handed on: its catalog cannot be
    /// sealed again, and the successor takes over from its final checkpoint.
    pub(super) fn handed_over(&mut self) {
        self.captures.uncaptured = None;
    }

    /// Makes the next checkpoint's capture due whatever the interval, as shutdown's final one is.
    pub(super) fn capture_next_checkpoint(&mut self) {
        self.captures.due = None;
    }

    /// Takes no capture until `capture_next_checkpoint`. For the recovery scenario, whose flow runs on
    /// past a cache to age it.
    #[cfg(test)]
    pub(super) fn defer_captures(&mut self) {
        self.captures.due = Some(Millis::MAX);
    }

    /// Seals the catalog at this checkpoint and hands the capture to the history owner, which
    /// publishes the paired cache, when a capture is due; otherwise keeps it for shutdown.
    /// Error cases:
    /// - the authoritative log cannot be synced
    fn capture_snapshot(&mut self, checkpoint: Checkpoint) -> anyhow::Result<()> {
        self.captures.uncaptured = None;
        if self.sealing.is_none() || self.progress.genesis.ensure_closed().is_err() {
            return Ok(());
        }
        let Some(base) = self.snapshot_base.clone() else {
            // A missing finalized witness cannot become a digest or zero accounting.
            return Ok(());
        };
        let context = self.recovery_context(base);
        let deferred = Deferred { checkpoint, progress: self.progress.clone(), context };
        if self.captures.due.is_some_and(|due| self.engine.now() < due) {
            self.captures.uncaptured = Some(deferred);
            return Ok(());
        }
        self.capture(deferred)
    }

    /// Captures a checkpoint with the recovery facts it had when it was written, which custody
    /// status may have moved on since; with a capture in flight, keeps it for shutdown instead, and
    /// the capture stays due. No command has been applied since the checkpoint.
    /// Nothing is captured once the engine holds a withdrawal approval: a cache does not carry the
    /// order approvals are paid in, which only full replay validates. The caches written before the
    /// first approval stay, so such a node restarts from the newest of them, or by full replay when
    /// none is left, and its restart time grows with the log written since.
    /// Error cases:
    /// - the authoritative log cannot be synced
    pub(super) fn capture(&mut self, deferred: Deferred) -> anyhow::Result<()> {
        let Some(endpoint) = self.sealing.clone() else { return Ok(()) };
        if self.engine.withdrawal_approvals().next().is_some() {
            self.captures.uncaptured = None;
            return Ok(());
        }
        let Some(permit) = endpoint.try_permit() else {
            // Shutdown captures it after all if no later checkpoint follows.
            self.captures.uncaptured = Some(deferred);
            return Ok(());
        };
        #[cfg(test)]
        let started = std::time::Instant::now();
        // A full queue, behind a lagging history index, would refuse the handover: nothing is synced,
        // sealed or encoded for it, and the capture stays due. One prepared in the slot is taken.
        let slot = match endpoint.reserve() {
            Ok(slot) => slot,
            Err(reason) => {
                #[cfg(test)]
                self.captures.held.push(started.elapsed());
                endpoint
                    .write_alert
                    .failed(|| warn!(alert = "snapshot_write_failed", "checkpoint remains in log: {reason:#}"));
                return Ok(());
            }
        };
        let Deferred { checkpoint, progress, context } = deferred;
        self.sync_log(false)?;
        let prepared = (|| -> anyhow::Result<()> {
            let (catalog, catalog_pin) = self.log.seal_catalog_boundary_unpublished()?;
            let snapshot = self.snapshot_fields(checkpoint, progress, context, catalog.checkpoint.offset)?;
            let (engine, job) = self.encoding(snapshot.checkpoint.seq)?;
            slot.submit(crate::snapshot::sealing::Capture {
                snapshot,
                engine,
                ts: self.last_ts,
                catalog_store: endpoint.catalog_store,
                catalog,
                catalog_pin,
                _permit: permit,
            });
            // Staged only once the history owner took the capture: a refused one, as when the owner's
            // queue is full behind a lagging index, costs the worker no encoding.
            if let (Some(job), Some(stats)) = (job, self.marks.stats()) {
                stats.stage_job(job);
            }
            Ok(())
        })();
        #[cfg(test)]
        self.captures.held.push(started.elapsed());
        match prepared {
            Ok(()) => self.captures.due = Some(self.engine.now().saturating_add(endpoint.interval_ms)),
            Err(reason) => endpoint
                .write_alert
                .failed(|| warn!(alert = "snapshot_write_failed", "checkpoint remains in log: {reason:#}")),
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
            base_work: self.snapshot_anchor_work.clone(),
            work: self.snapshot_work.clone(),
        }
    }

    /// The engine at the checkpoint after the command at `seq`, the last applied: due from the stats
    /// worker's copy, with the job that asks it, for the caller to stage, or encoded here when no
    /// worker runs.
    /// Error cases:
    /// - the engine cannot be encoded
    fn encoding(&self, seq: u64) -> anyhow::Result<(Encoding, Option<Job>)> {
        if let Some((job, pending)) = self.marks.stats().and_then(|stats| stats.encode_job(seq, self.engine.now())) {
            return Ok((Encoding::Pending(pending), Some(job)));
        }
        Ok((Encoding::Encoded(serde_json::to_string(&self.engine).context("encoding snapshot engine")?), None))
    }

    /// The cache `capture` hands over at this checkpoint, with the engine encoded here. For tests.
    /// Error cases:
    /// - as `snapshot_fields`, or the engine cannot be encoded
    #[cfg(test)]
    fn snapshot_payload(
        &self,
        checkpoint: Checkpoint,
        progress: snapshot::Progress,
        context: RecoveryContext,
        offset: crate::log::Offset,
    ) -> anyhow::Result<Snapshot> {
        let mut snapshot = self.snapshot_fields(checkpoint, progress, context, offset)?;
        snapshot.engine = serde_json::to_string(&self.engine).context("encoding snapshot engine")?;
        Ok(snapshot)
    }

    /// A cache's fields at this checkpoint, whose entry is at `offset`, with the progress and
    /// recovery facts it had when written, but the engine, which `encoding` gives.
    /// Error cases:
    /// - recovery context validation fails
    fn snapshot_fields(
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
            engine: String::new(),
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
#[cfg(test)]
mod encoding_tests;
