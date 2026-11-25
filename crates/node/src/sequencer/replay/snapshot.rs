//! Replay restored from a verified state snapshot instead of the full log. A cache that carries the replay
//! state a whole replay holds at its checkpoint (`Carried`) gives a replay from it the fold a whole replay
//! would give (`Fold::of`), while it continues from the cache's own payout state.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Context, ensure};
use engine_core::Engine;
use engine_core::types::AccountId;
use tracing::warn;

use crate::log::{CheckpointDomain, PrefixRing};
use crate::sequencer::SignedWithdrawal;
use crate::sequencer::replay::{Carried, Replay};
use crate::snapshot::{Progress, RecoveryBase, Snapshot};

/// The payout state a whole replay of the log holds at a cache's checkpoint, which the cache's own leaves out of
/// the approvals it no longer owed: every approval in payout order, those a withdrawal entry signed, and each one's
/// latest authorization.
pub(in crate::sequencer) struct Cached {
    payout_order: Vec<((AccountId, u64), usize)>,
    /// How many approvals the cache's own payout order starts with, the owed ones, which this order holds too.
    owed: usize,
    signed: BTreeSet<(AccountId, u64)>,
    authorized: BTreeMap<(AccountId, u64), SignedWithdrawal>,
}

/// The payout state a fold reads: payout order, signed approvals and latest authorizations.
pub(in crate::sequencer) type Payouts =
    (Vec<((AccountId, u64), usize)>, BTreeSet<(AccountId, u64)>, BTreeMap<(AccountId, u64), SignedWithdrawal>);

impl Cached {
    /// The payout state a whole replay holds where `replay`, from the cache, stands: this state, with what the
    /// replay took after the checkpoint.
    pub(in crate::sequencer) fn whole(&self, replay: &Replay) -> Payouts {
        let later = replay.payout_order.get(self.owed..).unwrap_or_default();
        let payout_order = self.payout_order.iter().chain(later).copied().collect();
        let signed = self.signed.union(&replay.signed).copied().collect();
        let mut authorized = self.authorized.clone();
        authorized.extend(replay.authorized.iter().map(|(key, withdrawal)| (*key, withdrawal.clone())));
        (payout_order, signed, authorized)
    }
}

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
            carried,
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
            partial: true,
            ..Self::default()
        };
        replay.prefix_ring.record(base.0, base.1);
        if let Some(carried) = carried {
            match replay.cached(&carried) {
                Ok(cached) => replay.take_carried(carried, cached),
                Err(e) => warn!(
                    seq = base.0,
                    "the cache's carried replay state is not its own, so its replay folds none: {e:#}"
                ),
            }
        }
        Ok(replay)
    }

    /// What a whole replay holds at this cache replay's checkpoint, from `carried`, beside the payout state the
    /// cache continues from.
    /// Error cases:
    /// - the state does not stand for the engine and the checkpoint (`Carried::check`)
    /// - it differs from the cache's command count and prefix, checkpoint count, accounting, genesis, activations,
    ///   parameters' seq or deferred approvals
    /// - the approvals the cache owes are not in its payout order in turn, one it signed is not signed there, or
    ///   their latest authorizations differ
    fn cached(&self, carried: &Carried) -> anyhow::Result<Cached> {
        let checkpoint = self.last_signed_checkpoint.as_ref().context("a cache replay stands at its checkpoint")?;
        carried.check(&self.engine, checkpoint)?;
        let encoded = |genesis| serde_json::to_vec(genesis).context("encoding the genesis progress");
        ensure!(
            (carried.commands, carried.command_prefix.0, carried.checkpoints, carried.accounting)
                == (self.commands, self.command_prefix, self.checkpoints, self.accounting)
                && (&carried.activations, carried.params_seq, &carried.deferred)
                    == (&self.activations, self.params_seq, &self.deferred)
                && encoded(&carried.genesis)? == encoded(&self.genesis)?,
            "the carried state is of another prefix than the cache's"
        );
        let owed: BTreeSet<_> = self.payout_order.iter().map(|(key, _)| *key).collect();
        let in_order: Vec<_> = carried.payout_order.iter().filter(|(key, _)| owed.contains(key)).copied().collect();
        ensure!(in_order == self.payout_order, "the cache's owed approvals are not in the carried payout order");
        ensure!(
            self.signed.is_subset(&carried.signed),
            "an approval the cache signed is not signed in the carried state"
        );
        let authorized: BTreeMap<_, _> = carried.authorized.iter().map(|w| ((w.account, w.nonce), w.clone())).collect();
        ensure!(
            self.authorized.iter().all(|(key, latest)| authorized.get(key) == Some(latest)),
            "an owed approval's latest authorization is not the carried state's"
        );
        let (payout_order, signed) = (carried.payout_order.clone(), carried.signed.clone());
        Ok(Cached { payout_order, owed: self.payout_order.len(), signed, authorized })
    }

    /// Takes `carried`, checked as `cached`, so this replay folds as a whole replay does: the latest commands'
    /// links, every checkpoint's signer and root, the turn takings and the checkpoint gate from it, and the payout
    /// state the cache leaves out kept apart (`Cached`).
    fn take_carried(&mut self, carried: Carried, cached: Cached) {
        let seq = self.last_signed_checkpoint.as_ref().map_or(0, |checkpoint| checkpoint.seq);
        let mut prefix_ring = PrefixRing::default();
        carried.links.iter().for_each(|link| prefix_ring.record(link.seq, link.link));
        let mut takings = carried.takings;
        // The replay verifies no range before the commands after the cache's.
        takings.set_applied_from(seq.saturating_add(1));
        self.prefix_ring = prefix_ring;
        self.recent = VecDeque::from(carried.links);
        self.signers_seen = carried.checkpoint_signers;
        self.payout_checkpoints.extend(carried.checkpoint_roots);
        self.takings = takings;
        self.requires_checkpoint_gate |= carried.requires_checkpoint_gate;
        self.cached = Some(Box::new(cached));
    }
}

#[cfg(test)]
mod tests;
