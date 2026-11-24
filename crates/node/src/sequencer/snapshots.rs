//! Checkpoint capture for paired recovery caches, and the daily snapshots published for a replay's ranges.
//! A capture is due every `[snapshots] interval` of sequenced time, and a daily snapshot at the first
//! checkpoint of each UTC day; the engine of either is encoded by the stats worker's copy, so the
//! sequencer's part is the log sync, the catalog seal and the recovery facts it already holds.

mod paired_recovery;

use anyhow::Context;
use engine_core::event::Event;
use engine_core::types::{AccountId, Millis};
use tracing::{warn, warn_span};

use crate::daily_snapshots::Job as DailyJob;
use crate::log::{Checkpoint, CheckpointDomain, Offset};
use crate::sequencer::Sequencer;
#[cfg(test)]
use crate::snapshot::sealing::CaptureStage;
use crate::snapshot::sealing::Encoding;
use crate::snapshot::{self, RecoveryContext, Snapshot};
use crate::stats::background::Job;

/// The least sequenced time a capture stays in flight, waiting for the history owner to take and store it,
/// before a capture behind it alerts. Outside shutdown, cadence also keeps the interval between the two.
const CAPTURE_LATE: Millis = 60_000;

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
    /// The checkpoint seq of the last capture handed to the history owner, which holds it until its index
    /// reaches the checkpoint, and the stamp at which it was handed over.
    handed: Option<(u64, Millis)>,
    /// Approvals mock custody has yet to sign, with their event indices: it signs them once the
    /// checkpoint covering them is written, so a capture of that checkpoint still owes them.
    unsigned: Vec<((AccountId, u64), usize)>,
    /// The writer of daily snapshots, under `[daily_snapshots]`.
    daily: Option<crate::daily_snapshots::Writer>,
    /// The replay state recovery folded, which the stats worker folds on from for the daily snapshots once it
    /// starts (`track_stats`).
    pub(super) fold: Option<crate::sequencer::replay::Fold>,
    /// How long each capture held the sequencer's thread, from its permit to its handover, whether
    /// the history owner took it or not. For the recovery scenario.
    #[cfg(test)]
    pub(super) held: Vec<std::time::Duration>,
}

impl Captures {
    /// No capture taken yet, and with `daily`, the directory of `[daily_snapshots]`, its writer started
    /// on a log whose last checkpoint is at `last_checkpoint`, and the replay state recovery folded, `fold`; a
    /// writer that does not start pages Warning `daily_snapshot_failed`, and the node runs on without one.
    pub(super) fn publishing(
        daily: Option<&std::path::Path>,
        last_checkpoint: Option<u64>,
        fold: Option<crate::sequencer::replay::Fold>,
    ) -> Self {
        let daily = daily.and_then(|dir| {
            crate::daily_snapshots::Writer::start(dir, last_checkpoint)
                .inspect_err(|e| {
                    warn!(
                        alert = "daily_snapshot_failed",
                        "the daily snapshot writer did not start, so none until a restart: {e:#}"
                    )
                })
                .ok()
        });
        Self { daily, fold, ..Self::default() }
    }
}

impl Sequencer {
    /// Captures a paired cache at this checkpoint, whose entry is at `offset`, when one is due, and hands
    /// the daily snapshot writer the engine at it when that is due, then collects retired catalog
    /// versions. Catalog collection runs here, once per checkpoint whether or not a capture was
    /// taken, instead of on every flush. A failed log sync fails closed; anything else only warns.
    /// Error cases:
    /// - the authoritative log cannot be synced
    pub(super) fn store_snapshot(&mut self, checkpoint: Checkpoint, offset: Offset) -> anyhow::Result<()> {
        self.daily_snapshot(&checkpoint, offset)?;
        self.capture_snapshot(checkpoint)?;
        self.maintain_catalog();
        Ok(())
    }

    /// Hands the daily snapshot writer the engine at this checkpoint, whose entry is at `offset`, when a
    /// snapshot is due: at the first checkpoint of a UTC day by sequenced time, once genesis has applied in
    /// full. A writer still busy with the last one skips the day before the engine is encoded. The stats
    /// worker encodes the engine only once the batch holding this checkpoint is durable; encoded here, the log
    /// is synced first. So a snapshot never names a checkpoint a crash could lose.
    /// Error cases:
    /// - the authoritative log cannot be synced
    fn daily_snapshot(&mut self, checkpoint: &Checkpoint, offset: Offset) -> anyhow::Result<()> {
        let (ts, seq) = (self.engine.now(), checkpoint.seq);
        let genesis = self.progress.genesis.ensure_closed().is_ok();
        match self.captures.daily.as_mut() {
            Some(daily) if genesis && daily.due(ts) && !daily.ready() => {
                daily.skip(seq, ts, "the writer is still writing the last one, or has stopped");
                return Ok(());
            }
            Some(daily) if genesis && daily.due(ts) => {}
            _ => return Ok(()),
        }
        // The replay state at the checkpoint comes from the stats worker's fold, behind the checkpoint's entry.
        let carried = self.marks.stats().and_then(|stats| stats.carry_job(seq));
        let encoded = match carried {
            Some(carried) => self.encoding(seq).map(|engine| (engine, carried)),
            None => Err(anyhow::anyhow!("no stats worker folds the replay state")),
        };
        let ((engine, job), (carry, carried)) = match encoded {
            Ok(encoded) => encoded,
            Err(e) => {
                warn!(alert = "daily_snapshot_failed", "no daily snapshot at seq {seq}: {e:#}");
                if let Some(daily) = self.captures.daily.as_mut() {
                    daily.encoding_failed();
                }
                return Ok(());
            }
        };
        match (job, self.marks.stats()) {
            (Some(job), Some(stats)) => stats.stage_job(job),
            _ => self.sync_log(false)?,
        }
        if let Some(stats) = self.marks.stats() {
            stats.stage_job(carry);
        }
        let carried = Encoding::Pending(carried);
        let history = self.history_anchors();
        // The key that signed the checkpoint signs the snapshot, in its deployment.
        let domain = crate::log::CheckpointDomain { chain_id: self.chain_id, vault: self.vault };
        let signing = crate::daily_snapshots::Signing { key: self.key.clone(), domain };
        if let Some(daily) = self.captures.daily.as_mut() {
            daily.take(DailyJob { offset, checkpoint: checkpoint.clone(), ts, engine, carried, history, signing });
        }
        Ok(())
    }

    /// The history anchors a daily snapshot of this checkpoint carries.
    fn history_anchors(&self) -> Encoding {
        Encoding::Encoded("{}".to_owned())
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
    /// status may have moved on since; with a capture in flight, the history owner stopped, or a step failing, keeps it
    /// for shutdown instead, and the capture stays due. An owner behind the log takes the capture and pairs it with
    /// the history it has indexed (`history::runtime`). No command has been applied since the checkpoint, and no
    /// withdrawal entry has been logged after it (`record`), so the payouts it owes are taken now.
    /// Error cases:
    /// - the authoritative log cannot be synced
    pub(super) fn capture(&mut self, deferred: Deferred) -> anyhow::Result<()> {
        let Some(endpoint) = self.sealing.clone() else { return Ok(()) };
        let Some(permit) = endpoint.try_permit() else {
            // A capture still in flight a minute after it was handed over waits for a history owner that has
            // not taken it, or is still storing it: no cache is written meanwhile.
            if let Some((seq, handed)) = self.captures.handed
                && self.engine.now().saturating_sub(handed) >= CAPTURE_LATE
            {
                let waited = self.engine.now().saturating_sub(handed) / 1_000;
                endpoint.write_alert.failed(|| {
                    warn_span!("capture", seq).in_scope(|| {
                        warn!(
                            alert = "snapshot_write_failed",
                            "checkpoint remains in log: the history owner has not stored the last capture after \
                             {waited} s"
                        )
                    })
                });
            }
            // Shutdown captures it after all if no later checkpoint follows.
            self.captures.uncaptured = Some(deferred);
            return Ok(());
        };
        #[cfg(test)]
        let started = std::time::Instant::now();
        // A stopped owner would refuse the handover: nothing is synced, sealed or encoded for it, and the
        // capture stays due. One prepared in the slot is taken.
        let slot = match endpoint.reserve() {
            Ok(slot) => slot,
            Err(reason) => {
                #[cfg(test)]
                self.captures.held.push(started.elapsed());
                endpoint
                    .write_alert
                    .failed(|| warn!(alert = "snapshot_write_failed", "checkpoint remains in log: {reason:#}"));
                // Shutdown tries it again if no later checkpoint follows.
                self.captures.uncaptured = Some(deferred);
                return Ok(());
            }
        };
        // A failure from here on leaves the checkpoint uncaptured, as a refusal does: shutdown, or the
        // next due capture, takes it.
        #[cfg(test)]
        let synced = self.fault(&endpoint, CaptureStage::Sync).and_then(|()| self.sync_log(false));
        #[cfg(not(test))]
        let synced = self.sync_log(false);
        if let Err(reason) = synced {
            self.captures.uncaptured = Some(deferred);
            return Err(reason);
        }
        let prepared = (|| -> anyhow::Result<u64> {
            #[cfg(test)]
            self.fault(&endpoint, CaptureStage::Seal)?;
            let (catalog, catalog_pin) = self.log.seal_catalog_boundary_unpublished()?;
            #[cfg(test)]
            self.fault(&endpoint, CaptureStage::Fields)?;
            let snapshot = self.snapshot_fields(
                deferred.checkpoint.clone(),
                deferred.progress.clone(),
                deferred.context.clone(),
                catalog.checkpoint.offset,
            )?;
            #[cfg(test)]
            self.fault(&endpoint, CaptureStage::Encode)?;
            let seq = snapshot.checkpoint.seq;
            let (engine, job) = self.encoding(seq)?;
            slot.submit(crate::snapshot::sealing::Capture {
                snapshot,
                engine,
                ts: self.last_ts,
                catalog_store: endpoint.catalog_store,
                catalog,
                catalog_pin,
                _permit: permit,
                #[cfg(test)]
                encode_polls: endpoint.encode_polls.clone(),
            });
            // Staged only once the capture is in the owner's slot: a refused one, as when the owner has
            // stopped, costs the worker no encoding.
            if let (Some(job), Some(stats)) = (job, self.marks.stats()) {
                stats.stage_job(job);
            }
            Ok(seq)
        })();
        #[cfg(test)]
        self.captures.held.push(started.elapsed());
        match prepared {
            Ok(seq) => {
                self.captures.handed = Some((seq, self.engine.now()));
                self.captures.due = Some(self.engine.now().saturating_add(endpoint.interval_ms));
            }
            Err(reason) => {
                endpoint
                    .write_alert
                    .failed(|| warn!(alert = "snapshot_write_failed", "checkpoint remains in log: {reason:#}"));
                self.captures.uncaptured = Some(deferred);
            }
        }
        Ok(())
    }

    /// Fails `stage` of this capture when a test asked its endpoint to.
    #[cfg(test)]
    fn fault(&self, endpoint: &crate::snapshot::sealing::Endpoint, stage: CaptureStage) -> anyhow::Result<()> {
        anyhow::ensure!(!endpoint.take_fault(stage), "injected {stage:?} failure");
        Ok(())
    }

    /// The recovery facts at the checkpoint just written: concrete outstanding work, accounting,
    /// deferred withdrawals, and the finalized base. `snapshot_fields` adds the payouts owed.
    pub(super) fn recovery_context(&self, base: snapshot::RecoveryBase) -> RecoveryContext {
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
            payouts: None,
        }
    }

    /// The approvals still owed a delivery at `checkpoint`, which the log ends at, in payout order,
    /// with the roots of the earlier checkpoints their proofs were taken or are being taken at; `None`
    /// while the engine holds no approval. The gate holds each unpaid approval and its latest
    /// authorization; without one, mock custody owes only the approvals it has yet to sign. Built only
    /// for a capture that is taken, it costs a clone of each owed approval's latest authorization.
    fn owed_payouts(&self, checkpoint: &Checkpoint) -> Option<snapshot::Payouts> {
        self.engine.withdrawal_approvals().next()?;
        let mut owed: Vec<_> = match &self.gate {
            Some(gate) => gate
                .payouts
                .iter()
                .map(|(&key, payout)| (key, payout.order.1, payout.latest.clone()))
                .chain(gate.backlog.iter().map(|(key, index, latest)| (*key, *index, latest.clone())))
                .collect(),
            None => self.captures.unsigned.iter().map(|&(key, index)| (key, index, None)).collect(),
        };
        owed.sort_by_cached_key(|&((account, nonce), index, _)| {
            let approval = self.engine.withdrawal_approval(account, nonce).expect("an owed payout is an approval");
            (approval.approved_seq, index)
        });
        let owed = owed.into_iter().map(|((account, nonce), event_index, latest)| snapshot::Owed {
            account,
            nonce,
            event_index,
            latest,
        });
        let proofs = self.gate.iter().flat_map(|gate| gate.payouts.values().filter_map(|payout| payout.proof.as_ref()));
        let roots = proofs.map(|proof| (proof.seq, proof.root)).chain(self.roots.proving());
        let roots = roots.filter(|(seq, _)| *seq < checkpoint.seq).collect();
        Some(snapshot::Payouts { owed: owed.collect(), roots })
    }

    /// Notes the approvals among a command's `events` that mock custody signs once their checkpoint
    /// is written, so a capture of that checkpoint owes them. A gate holds its own.
    pub(super) fn note_unsigned(&mut self, events: &[Event]) {
        if self.gate.is_some() {
            return;
        }
        for (index, event) in events.iter().enumerate() {
            if let Event::WithdrawalApproved { account, nonce, .. } = event
                && self.engine.withdrawal_approval(*account, *nonce).is_some()
            {
                self.captures.unsigned.push(((*account, *nonce), index));
            }
        }
    }

    /// Notes the approvals recovery found unsigned, `unreleased`, with their indices in `order`, as
    /// `note_unsigned` does for a command's, and gives them back for mock custody to sign.
    pub(super) fn owe_unsigned(
        &mut self,
        unreleased: Vec<engine_core::withdrawal::WithdrawalApproval>,
        order: &[((AccountId, u64), usize)],
    ) -> Vec<engine_core::withdrawal::WithdrawalApproval> {
        let unsigned =
            |key: &(AccountId, u64)| unreleased.iter().any(|approval| (approval.account, approval.nonce) == *key);
        self.captures.unsigned.extend(order.iter().filter(|(key, _)| unsigned(key)));
        unreleased
    }

    /// Forgets an approval mock custody has signed: a later checkpoint owes it nothing.
    pub(super) fn note_signed(&mut self, key: (AccountId, u64)) {
        self.captures.unsigned.retain(|(unsigned, _)| *unsigned != key);
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
    pub(super) fn snapshot_payload(
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
    /// recovery facts it had when written and the payouts it owes, but the engine, which `encoding`
    /// gives.
    /// Error cases:
    /// - recovery context validation fails
    fn snapshot_fields(
        &self,
        checkpoint: Checkpoint,
        progress: snapshot::Progress,
        context: RecoveryContext,
        offset: crate::log::Offset,
    ) -> anyhow::Result<Snapshot> {
        let context = RecoveryContext { payouts: self.owed_payouts(&checkpoint), ..context };
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
mod capture_failure_tests;
#[cfg(test)]
mod context_tests;
#[cfg(test)]
mod custody_tests;
#[cfg(test)]
mod encoding_tests;
