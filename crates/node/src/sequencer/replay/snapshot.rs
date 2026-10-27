//! Replay restored from a verified state snapshot instead of the full log.

use std::collections::BTreeMap;

use anyhow::{Context, ensure};
use engine_core::Engine;
use engine_core::types::AccountId;

use crate::log::CheckpointDomain;
use crate::sequencer::replay::Replay;
use crate::snapshot::{Progress, RecoveryBase, Snapshot};

impl Replay {
    /// Matches a separately signed complete history head to this actual strict replay prefix.
    /// Error cases:
    /// - command, time or canonical command/event prefix differs
    pub(super) fn verify_history_head(&self, head: &crate::history::authenticated::Head) -> anyhow::Result<()> {
        ensure!(
            head.last_seq() == self.engine.last_seq() && head.last_ts() == Some(self.engine.now()),
            "history head does not match the replay cursor"
        );
        ensure!(head.command_prefix == self.command_prefix, "history head does not match the replayed command prefix");
        Ok(())
    }

    /// Restores required concrete replay facts with fresh external signer and finality authority.
    /// Error cases:
    /// - the cache contradicts its context, deployment, signer, or finalized prefix
    pub(in crate::sequencer) fn from_snapshot(
        engine: Engine,
        snapshot: Snapshot,
        domain: CheckpointDomain,
        trusted_signer: Option<AccountId>,
        parent: Option<crate::log::CheckpointParent>,
    ) -> anyhow::Result<Self> {
        snapshot.context.validate(&snapshot.checkpoint, domain)?;
        snapshot.context.validate_engine(&snapshot.checkpoint, &engine)?;
        crate::sequencer::legacy::check_snapshot_provenance(
            &engine,
            &snapshot.progress.activations,
            &snapshot.context.deferred,
        )?;
        // TODO: a snapshot does not carry payout delivery order yet, so a log that approved a
        // withdrawal recovers by full replay, which validates every approval's event order.
        ensure!(
            engine.withdrawal_approvals().next().is_none(),
            "snapshot recovery cannot restore withdrawal payout order; the full log is replayed instead"
        );
        ensure!(
            trusted_signer
                .is_none_or(|signer| { snapshot.context.checkpoint_signers.iter().all(|named| *named == signer) }),
            "snapshot checkpoint signer differs from trusted recovery authority"
        );
        let work = parent.map(|parent| snapshot.context.after_anchor(parent)).transpose()?.unwrap_or_default();
        let recovery_parent = work
            .last()
            .map(|work| crate::log::CheckpointParent {
                has_checkpoint: true,
                seq: work.checkpoint.seq,
                root: work.checkpoint.root.parse().expect("snapshot work is signature checked"),
            })
            .or(parent);
        let recovery_anchor =
            parent.filter(|parent| parent.has_checkpoint).and_then(|parent| match &snapshot.context.base {
                RecoveryBase::Checkpoint { checkpoint } if checkpoint.seq == parent.seq => Some(checkpoint.clone()),
                _ => snapshot
                    .context
                    .work
                    .iter()
                    .find(|work| work.checkpoint.seq == parent.seq)
                    .map(|work| work.checkpoint.clone()),
            });
        let recovery_anchor_work = if let Some(parent) = parent.filter(|parent| parent.has_checkpoint) {
            Some(
                snapshot
                    .context
                    .work
                    .iter()
                    .find(|work| work.checkpoint.seq == parent.seq)
                    .cloned()
                    .or_else(|| {
                        (snapshot.checkpoint.seq == parent.seq).then(|| {
                            crate::checkpoints::Work::capture(
                                snapshot.checkpoint.clone(),
                                snapshot.context.accounting,
                                &engine,
                            )
                        })
                    })
                    .context("snapshot lacks finalized anchor accounting; full replay is required")?,
            )
        } else {
            None
        };
        let Snapshot { progress: Progress { genesis, activations, checkpoints }, context, checkpoint, .. } = snapshot;
        let last_checkpoint_ts = Some(engine.now());
        // Commands after the snapshot's checkpoint are sealed from that checkpoint's link.
        let mut origin = crate::sequencer::origin::Origin::new(Some(domain), trusted_signer);
        origin.link = crate::log::command_seal::checkpoint_link(&checkpoint)?;
        let requires_checkpoint_gate = checkpoint.parent.is_some();
        Ok(Self {
            legacy_deposits: BTreeMap::new(),
            legacy_approvals: crate::sequencer::legacy::Approvals::default(),
            engine,
            commands: context.commands,
            command_prefix: context.command_prefix,
            checkpoints,
            activations,
            genesis,
            last_checkpoint_ts,
            approved: context.unsigned.into(),
            logs_events: context.logs_events,
            accounting: context.accounting,
            deferred: context.deferred,
            checkpoint_signers: context.checkpoint_signers,
            recovery_work: work,
            recovery_anchor_work,
            recovery_parent,
            recovery_origin: parent,
            recovery_anchor,
            last_signed_checkpoint: Some(checkpoint),
            domain: Some(domain),
            trusted_signer,
            origin,
            requires_checkpoint_gate,
            ..Self::default()
        })
    }
}
