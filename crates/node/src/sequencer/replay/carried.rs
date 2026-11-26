//! The replay state a daily snapshot carries besides the engine: what a replay of the log holds just after the
//! snapshot's checkpoint entry and folds from every entry before it. A start from the snapshot takes it in
//! place of a pass over the log before the checkpoint (`Replay::resume`), and a replay that passes the
//! checkpoint gives its own to compare (`Replay::carried`), as the stats worker's fold gives it for the
//! snapshots the node writes (`stats::fold`).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Context, ensure};
use engine_core::Engine;
use engine_core::Fixed;
use engine_core::eip712::B256;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checkpoints::accounting::Accounting;
use crate::log::{Checkpoint, Offset, PrefixRing};
use crate::log_check::Takings;
use crate::sequencer::replay::live::Link;
use crate::sequencer::replay::{Fold, Replay};
use crate::sequencer::{GenesisProgress, SignedWithdrawal, legacy};

/// What a replay of a log holds after a checkpoint entry, besides the engine.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Carried {
    /// The commands the log holds through the checkpoint's.
    pub commands: u64,
    /// The cumulative command-prefix digest after the checkpoint's command.
    pub command_prefix: B256,
    /// The latest commands' links, oldest first, as many as `PrefixRing::CAPACITY`, the last the checkpoint's.
    pub links: Vec<Link>,
    pub checkpoints: u64,
    pub accounting: Accounting,
    pub genesis: GenesisProgress,
    pub activations: Vec<(ProtocolVersion, u64)>,
    pub params_seq: Option<u64>,
    /// Every checkpoint's signer.
    pub checkpoint_signers: BTreeSet<AccountId>,
    /// Every checkpoint's root, by seq: a typed authorization after the checkpoint may name any.
    pub checkpoint_roots: BTreeMap<u64, B256>,
    /// The log's turn takings, collected from its first command.
    pub takings: Takings,
    /// The approvals the engine keeps, in payout order, each with its event's index in its command.
    pub payout_order: Vec<((AccountId, u64), usize)>,
    /// The approvals a withdrawal entry signed.
    pub signed: BTreeSet<(AccountId, u64)>,
    /// Each approval's latest typed authorization, in key order.
    pub authorized: Vec<SignedWithdrawal>,
    /// The approvals a checkpoint deferred that no withdrawal entry has signed: seq, account, amount, nonce.
    pub deferred: Vec<(u64, AccountId, Fixed, u64)>,
    pub requires_checkpoint_gate: bool,
    /// The sequenced time of the access source's latest failure while a failure episode is open, which the first
    /// fresh decision after it resolves (`sequencer::access::AccessAlerts`).
    #[serde(default)]
    pub access_failed_since: Option<engine_core::types::Millis>,
}

impl Carried {
    /// The SHA-256 of the state's encoding, by which two folds of one log are compared.
    /// Error cases:
    /// - the state cannot be encoded
    pub(crate) fn digest(&self) -> anyhow::Result<[u8; 32]> {
        Ok(Sha256::digest(serde_json::to_vec(self).context("encoding the carried replay state")?).into())
    }

    /// The key that sealed the latest sealed command the links hold.
    pub(crate) fn last_sealer(&self) -> Option<AccountId> {
        self.links.iter().rev().find_map(|link| link.sealer)
    }

    /// Checks the state against `engine`, the snapshot's, and `checkpoint`, its checkpoint, before a replay
    /// goes on from them.
    /// Error cases:
    /// - the engine stands at another seq, or at another protocol version than the last activation's
    /// - the state names no command, logs no checkpoint at the checkpoint's seq with its root and signer, or its
    ///   links do not end at the checkpoint's command with the command prefix
    /// - an approval the engine holds is not in payout order, or one there is not the engine's
    /// - a deferred approval is after the checkpoint, or the state is not one a snapshot may stand for
    ///   (`legacy::check_snapshot_provenance`)
    pub(crate) fn check(&self, engine: &Engine, checkpoint: &Checkpoint) -> anyhow::Result<()> {
        ensure!(
            engine.last_seq() == Some(checkpoint.seq),
            "the engine stands at seq {:?}, not at its checkpoint's, {}",
            engine.last_seq(),
            checkpoint.seq
        );
        ensure!(self.commands > 0, "the carried state names no command");
        ensure!(
            self.activations.last().is_none_or(|(version, _)| *version == engine.protocol_version()),
            "the engine is at protocol {:?}, not the last activation's",
            engine.protocol_version()
        );
        let root: B256 = checkpoint.root.parse().context("the checkpoint's root")?;
        ensure!(
            self.checkpoint_roots.get(&checkpoint.seq) == Some(&root),
            "the carried state lacks its checkpoint's root"
        );
        let signer: AccountId = checkpoint.signer.parse().context("the checkpoint's signer")?;
        ensure!(self.checkpoint_signers.contains(&signer), "the carried state lacks its checkpoint's signer");
        let last = self.links.last().context("the carried state holds no command link")?;
        ensure!(
            last.seq == checkpoint.seq && last.link == self.command_prefix && last.ts == engine.now(),
            "the carried links do not end at the checkpoint's command"
        );
        ensure!(
            self.links.windows(2).all(|pair| pair[0].seq.checked_add(1) == Some(pair[1].seq)),
            "the carried links are not of consecutive commands"
        );
        ensure!(self.links.len() <= PrefixRing::CAPACITY, "the carried state holds more links than a replay keeps");
        let approvals: BTreeSet<_> =
            engine.withdrawal_approvals().map(|approval| (approval.account, approval.nonce)).collect();
        let ordered: BTreeSet<_> = self.payout_order.iter().map(|(key, _)| *key).collect();
        ensure!(
            ordered == approvals && ordered.len() == self.payout_order.len(),
            "the carried payout order is not the engine's approvals"
        );
        ensure!(
            self.authorized.iter().all(|withdrawal| approvals.contains(&(withdrawal.account, withdrawal.nonce))),
            "a carried authorization is of no approval the engine holds"
        );
        ensure!(
            self.deferred.iter().all(|(seq, ..)| *seq <= checkpoint.seq),
            "a carried deferred approval is after the checkpoint"
        );
        legacy::check_snapshot_provenance(engine, &self.activations, &self.deferred)
    }
}

impl Replay {
    /// Whether this replay folds the whole log's replay state (`Fold::of`), as a replay from a recovery cache does
    /// only when the cache carries its own.
    pub(in crate::sequencer) fn folds(&self) -> bool {
        self.range.is_none() && (!self.partial || self.cached.is_some())
    }

    /// What this replay holds after the entry it applied last, which must be a checkpoint, for a daily
    /// snapshot to carry or to compare with one's.
    /// Error cases:
    /// - the replay is of one range of a log, or of a log whose commands carry no events (`Fold::of`), or the
    ///   entry it applied last is not a checkpoint
    pub(in crate::sequencer) fn carried(&self) -> anyhow::Result<Carried> {
        let seq = self.last_signed_checkpoint.as_ref().map(|checkpoint| checkpoint.seq);
        Fold::of(self)?.carried(seq.context("the replay holds no checkpoint")?)
    }

    /// This replay, set up with its trust, going on from a daily snapshot: `engine` at `checkpoint`, the entry
    /// at `offset`, with what `carried` says the log before it folds to, as a replay of that log would hold
    /// them after the checkpoint, the engine's root checked against the checkpoint's as the replay checks a
    /// checkpoint's root. Its seals are checked from the checkpoint's link on. A recovery anchored at the
    /// vault's finalized checkpoint takes the snapshot's as its anchor when they are one.
    /// Error cases:
    /// - the state does not stand for the engine and the checkpoint (`Carried::check`)
    /// - the engine's root is not the checkpoint's
    /// - a recovery's finalized checkpoint is before the snapshot's, which then needs custody work the snapshot
    ///   does not carry
    pub(in crate::sequencer) fn resume(
        mut self,
        engine: Engine,
        carried: Carried,
        checkpoint: Checkpoint,
        offset: Offset,
    ) -> anyhow::Result<Self> {
        carried.check(&engine, &checkpoint)?;
        self.origin.link = crate::log::command_seal::checkpoint_link(&checkpoint)?;
        let mut takings = carried.takings;
        // The replay verifies no range before the commands after the snapshot's.
        takings.set_applied_from(checkpoint.seq.saturating_add(1));
        self.resumed_after = Some(offset);
        self.access_alerts = crate::sequencer::access::AccessAlerts::resumed(carried.access_failed_since);
        let mut prefix_ring = PrefixRing::default();
        carried.links.iter().for_each(|link| prefix_ring.record(link.seq, link.link));
        self.carried_links = carried.links.clone();
        self.last_marks = engine.markets().iter().filter_map(|(id, market)| Some((*id, market.mark?))).collect();
        self.last_checkpoint_ts = Some(engine.now());
        if let Some(parent) = self.recovery_origin.filter(|parent| parent.has_checkpoint) {
            ensure!(
                parent.seq >= checkpoint.seq,
                "the vault's finalized checkpoint, at seq {}, is before the snapshot's, at seq {}",
                parent.seq,
                checkpoint.seq
            );
            if parent.seq == checkpoint.seq && parent.root.to_string() == checkpoint.root {
                self.recovery_anchor = Some(checkpoint.clone());
                self.recovery_anchor_ts = Some(engine.now());
                self.recovery_anchor_work =
                    Some(crate::checkpoints::Work::capture(checkpoint.clone(), carried.accounting, &engine));
            }
        }
        let (seq, root) = (checkpoint.seq, checkpoint.root.clone());
        let mut resumed = Self {
            engine,
            commands: carried.commands,
            command_prefix: carried.command_prefix.0,
            recent: VecDeque::from(carried.links),
            prefix_ring,
            checkpoints: carried.checkpoints,
            last_checkpoint_offset: Some(offset),
            last_offset: Some(offset),
            last_signed_checkpoint: Some(checkpoint),
            accounting: carried.accounting,
            genesis: carried.genesis,
            activations: carried.activations,
            params_seq: carried.params_seq,
            signers_seen: carried.checkpoint_signers.clone(),
            checkpoint_signers: carried.checkpoint_signers,
            payout_checkpoints: carried.checkpoint_roots,
            takings,
            payout_order: carried.payout_order,
            signed: carried.signed,
            authorized: carried.authorized.into_iter().map(|w| ((w.account, w.nonce), w)).collect(),
            deferred: carried.deferred,
            requires_checkpoint_gate: carried.requires_checkpoint_gate,
            logs_events: true,
            ..self
        };
        resumed.roots.checkpoint(&mut resumed.engine, seq, &root)?;
        Ok(resumed)
    }
}

/// The replay state a whole replay of the log in `dir`, a copy in `domain` under `attested`, carries after its
/// checkpoint at `offset`, encoded as a daily snapshot carries it: for a test that writes a snapshot there.
/// Error cases:
/// - the log cannot be read or replayed to `offset`, or holds no checkpoint there
#[cfg(test)]
pub(crate) fn carried_at(
    dir: &std::path::Path,
    offset: Offset,
    (domain, attested): (crate::log::CheckpointDomain, BTreeSet<AccountId>),
) -> anyhow::Result<String> {
    let mut replay = Replay::copy(domain, attested);
    let read = crate::log::SegmentedLog::open(dir, |entry| {
        replay.apply(&entry)?;
        ensure!(replay.last_offset != Some(offset), crate::log::Stop);
        Ok(())
    });
    match read {
        Err(e) if e.is::<crate::log::Stop>() => {}
        Err(e) => return Err(e),
        Ok(_) => anyhow::bail!("the log ends before {offset}"),
    }
    Ok(serde_json::to_string(&replay.carried()?)?)
}
