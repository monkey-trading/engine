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
    pub(in crate::sequencer) fn verify_history_head(
        &self,
        head: &crate::history::authenticated::Head,
    ) -> anyhow::Result<()> {
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
        // The approvals still owed, their latest authorizations, and the roots a later one names, as
        // the log before the checkpoint gave them; replay checks every withdrawal entry after it.
        let payouts = snapshot.context.restore_payouts(&snapshot.checkpoint, &engine)?;
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
        let recovery_anchor_work = if let Some(parent) =
            parent.filter(|parent| parent.has_checkpoint && parent.seq <= snapshot.checkpoint.seq)
        {
            Some(
                snapshot
                    .context
                    .work
                    .iter()
                    .chain(snapshot.context.base_work.iter())
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
        let Snapshot {
            progress: Progress { genesis, activations, checkpoints, params_seq },
            context,
            checkpoint,
            offset,
            ..
        } = snapshot;
        let last_checkpoint_ts = Some(engine.now());
        // The base's own command is the snapshot engine's last: its time is the anchor's when the
        // vault finalized the base. An anchor under the base is the anchor pass's to stamp.
        let recovery_anchor_ts =
            recovery_anchor.as_ref().filter(|anchor| anchor.seq == checkpoint.seq).map(|_| engine.now());
        // The pinned snapshot envelope signs all engine bytes, including legacy staking references.
        // Commands after its checkpoint are checked independently from that checkpoint's link.
        let mut origin = crate::sequencer::origin::Origin::new(Some(domain), trusted_signer);
        origin.link = crate::log::command_seal::checkpoint_link(&checkpoint)?;
        let requires_checkpoint_gate = checkpoint.parent.is_some();
        // The base's prefix is authenticated with the snapshot: with no command after it, a member's
        // status at the base's seq still has a prefix to match.
        let base = (checkpoint.seq, engine_core::eip712::B256::from(context.command_prefix));
        let mut replay = Self {
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
            payout_order: payouts.payout_order,
            delivered: payouts.delivered,
            authorized: payouts.authorized,
            signed: payouts.signed,
            payout_checkpoints: payouts.roots,
            logs_events: context.logs_events,
            accounting: context.accounting,
            deferred: context.deferred,
            checkpoint_signers: context.checkpoint_signers,
            recovery_work: work,
            recovery_anchor_work,
            recovery_parent,
            recovery_origin: parent,
            recovery_anchor_ts,
            recovery_anchor,
            last_signed_checkpoint: Some(checkpoint),
            last_checkpoint_offset: Some(offset),
            last_offset: Some(offset),
            params_seq,
            domain: Some(domain),
            trusted_signer,
            origin,
            requires_checkpoint_gate,
            ..Self::default()
        };
        replay.prefix_ring.record(base.0, base.1);
        Ok(replay)
    }
}
