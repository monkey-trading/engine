//! A member's pass over its copy before a daily snapshot's checkpoint, which applies nothing. It checks what a
//! whole replay of the copy checks without the engine: every command's link and seal (`Origin`, ahead of the
//! pass on the replay's workers), every checkpoint's signature by one of the vault's recorded signers and its
//! seq, and every typed withdrawal entry's authorization; and it folds, from the commands and the events their
//! seals cover, what the replay keeps across the snapshot besides the engine: the command prefix and its ring,
//! checkpoint accounting, genesis progress, the activations, the latest applied parameters, the checkpoint
//! signers and roots, the turn takings, the approvals in payout order with their latest authorizations, and the
//! approvals a checkpoint deferred. From `V22` the engine keeps every approval, so the snapshot's approvals name
//! which approval events enter payout order, as the engine's own did when each command applied.
//!
//! What it takes on trust is the state at the snapshot's checkpoint: the vault's recorded signer that signed the
//! checkpoint, and the snapshot over its whole state, vouches that the commands before it produce it. The
//! replica's replay applies the snapshot's checkpoint first (`into_live`), which computes the snapshot's root over
//! every key and compares it with the checkpoint's.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Context, bail, ensure};
use engine_core::command::Command;
use engine_core::deposits::LegacyDeposit;
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};
use engine_core::{Engine, Fixed};

use crate::checkpoints::accounting::Accounting;
use crate::log::{self, Checkpoint, CheckpointDomain, CommandEntry, LogEntry, Offset, PrefixRing, ReplayWorkers};
use crate::sequencer::origin::Origin;
use crate::sequencer::replay::live::retained::Retained;
use crate::sequencer::replay::live::{Link, LiveReplay};
use crate::sequencer::replay::{Replay, ensure_signed};
use crate::sequencer::{GenesisProgress, SignedWithdrawal, legacy};

/// The pass over a copy up to the entry before a daily snapshot's checkpoint.
pub(crate) struct Before {
    domain: CheckpointDomain,
    attested: BTreeSet<AccountId>,
    /// The snapshot's approvals, each one's amount and approval seq by account and nonce.
    approvals: BTreeMap<(AccountId, u64), (Fixed, u64)>,
    origin: Origin,
    last_offset: Option<Offset>,
    last_seq: Option<u64>,
    last_ts: Option<Millis>,
    /// The version the last activation took effect at; the log's own until the first.
    version: Option<ProtocolVersion>,
    commands: u64,
    command_prefix: [u8; 32],
    prefix_ring: PrefixRing,
    checkpoints: u64,
    last_signed_checkpoint: Option<Checkpoint>,
    last_checkpoint_offset: Option<Offset>,
    last_checkpoint_ts: Option<Millis>,
    params_seq: Option<u64>,
    activations: Vec<(ProtocolVersion, u64)>,
    /// Approvals the last command made that the engine does not keep, awaiting their withdrawal entries.
    approved: VecDeque<(AccountId, Fixed, u64)>,
    logs_events: bool,
    genesis: GenesisProgress,
    deferred: Vec<(u64, AccountId, Fixed, u64)>,
    accounting: Accounting,
    legacy_deposits: BTreeMap<String, LegacyDeposit>,
    checkpoint_signers: BTreeSet<AccountId>,
    payout_checkpoints: BTreeMap<u64, B256>,
    takings: crate::log_check::Takings,
    payout_order: Vec<((AccountId, u64), usize)>,
    signed: BTreeSet<(AccountId, u64)>,
    authorized: BTreeMap<(AccountId, u64), SignedWithdrawal>,
    requires_checkpoint_gate: bool,
    last_sealer: Option<AccountId>,
    retained: Retained,
}

impl Before {
    /// A pass over a copy in `domain` whose checkpoints, typed withdrawals and command seals must be by one of
    /// `attested`, the vault's recorded signers, toward a daily snapshot whose engine is `snapshot`, with the
    /// seqs whose links and checkpoints a replica keeps (`LiveReplay::keep`) in `keep`.
    pub(crate) fn new(
        domain: CheckpointDomain,
        attested: BTreeSet<AccountId>,
        snapshot: &Engine,
        keep: &[u64],
    ) -> Self {
        let approvals = snapshot
            .withdrawal_approvals()
            .map(|approval| ((approval.account, approval.nonce), (approval.amount, approval.approved_seq)))
            .collect();
        let retained = Retained { keeping: keep.iter().copied().collect(), ..Retained::default() };
        Self {
            origin: Origin::copy(domain, attested.clone()),
            domain,
            attested,
            approvals,
            last_offset: None,
            last_seq: None,
            last_ts: None,
            version: None,
            commands: 0,
            command_prefix: [0; 32],
            prefix_ring: PrefixRing::default(),
            checkpoints: 0,
            last_signed_checkpoint: None,
            last_checkpoint_offset: None,
            last_checkpoint_ts: None,
            params_seq: None,
            activations: Vec::new(),
            approved: VecDeque::new(),
            logs_events: false,
            genesis: GenesisProgress::default(),
            deferred: Vec::new(),
            accounting: Accounting::default(),
            legacy_deposits: BTreeMap::new(),
            checkpoint_signers: BTreeSet::new(),
            payout_checkpoints: BTreeMap::new(),
            takings: crate::log_check::Takings::default(),
            payout_order: Vec::new(),
            signed: BTreeSet::new(),
            authorized: BTreeMap::new(),
            requires_checkpoint_gate: false,
            last_sealer: None,
            retained,
        }
    }

    /// Has the seals of the copy at `path` checked ahead of the pass by `workers`, from its first command
    /// (`Origin::check_ahead`); the replica the pass becomes goes on taking their verdicts.
    pub(crate) fn check_seals_ahead(&mut self, path: &std::path::Path, workers: ReplayWorkers) {
        self.origin.check_ahead(path, 0, workers);
    }

    /// Where the copy's next entry sits, `entry`, after the last the pass took.
    pub(crate) fn next_offset(&self, entry: &LogEntry) -> Option<Offset> {
        log::offset_of(self.last_offset, entry).ok()
    }

    /// Keeps the posted head `(seq, link)` once the link the pass computed at `seq` is that one
    /// (`LiveReplay::pin`).
    pub(crate) fn pin(&mut self, seq: u64, link: B256) -> Option<bool> {
        self.retained.pin(seq, link)
    }

    /// Takes the copy's next entry before the snapshot's checkpoint, checking it as a whole replay of the copy
    /// checks it but for what only its engine shows.
    /// Error cases:
    /// - a command out of sequence or stamped before the last, whose seal does not verify over its link or is
    ///   by none of the signers, that carries no logged events, or whose events the checkpoint accounting or a
    ///   custody finality activation's legacy credits refuse
    /// - a withdrawal entry that is not a typed authorization by one of the signers of an approval the snapshot
    ///   holds, made before it, naming a checkpoint the copy holds before it
    /// - a checkpoint at another seq than the last command's, whose signature does not verify or is by none of
    ///   the signers, or that covers a command no signer sealed, or a command whose approvals the engine does
    ///   not keep with no withdrawal entry before the next command or checkpoint
    pub(crate) fn entry(&mut self, entry: &LogEntry) -> anyhow::Result<()> {
        self.last_offset = log::offset_of(self.last_offset, entry).ok();
        match entry {
            LogEntry::Command(command) => self.command(command),
            LogEntry::Withdrawal(withdrawal) => self.withdrawal(withdrawal),
            LogEntry::Checkpoint(checkpoint) => self.checkpoint(checkpoint),
        }
    }

    /// Takes a command (see `entry`).
    fn command(&mut self, command: &CommandEntry) -> anyhow::Result<()> {
        ensure_signed(self.logs_events, &self.approved, self.last_seq)?;
        let (seq, ts) = (command.sequenced.seq, command.sequenced.ts);
        ensure!(
            self.last_seq.is_none_or(|last| Some(seq) == last.checked_add(1)),
            "seq {seq} follows {:?}",
            self.last_seq
        );
        ensure!(self.last_ts.is_none_or(|last| ts >= last), "seq {seq} is stamped before the command before it");
        // Before its first activation the copy's engine is at the version an empty engine holds.
        let version = self.version.unwrap_or_default();
        self.origin.command(command, version)?;
        let events = command.events.as_deref().with_context(|| format!("seq {seq} has no logged events"))?;
        let cmd = &command.sequenced.cmd;
        legacy::check_activation_ledger(cmd, &self.legacy_deposits)?;
        if let Some(version) = activated(cmd, events) {
            self.version = Some(version);
            self.activations.push((version, seq));
        }
        self.genesis.command(&command.sequenced, events);
        if crate::sequencer::house::applied_set_params(cmd, events) {
            self.params_seq = Some(seq);
        }
        self.accounting.apply(cmd, events)?;
        // A credit counts as legacy under a version the copy activated before custody finality: a copy whose
        // first command follows a state no log holds, as a test's does, starts past it.
        if self.version.is_some()
            && !version.supports_deposit_finality()
            && let Some(deposit) = LegacyDeposit::from_command(cmd, events)
        {
            self.legacy_deposits.insert(deposit.chain_ref.clone(), deposit);
        }
        self.takings.stamped(seq, ts);
        self.takings.command(seq, events);
        let kept = |account: &AccountId, nonce: &u64| {
            self.approvals.get(&(*account, *nonce)).is_some_and(|(_, approved_seq)| *approved_seq == seq)
        };
        for (index, event) in events.iter().enumerate() {
            if let Event::WithdrawalApproved { account, nonce, .. } = event
                && kept(account, nonce)
            {
                self.payout_order.push(((*account, *nonce), index));
            }
        }
        self.approved = events
            .iter()
            .filter_map(|event| match event {
                Event::WithdrawalApproved { account, amount, nonce } if !kept(account, nonce) => {
                    Some((*account, *amount, *nonce))
                }
                _ => None,
            })
            .collect();
        self.logs_events = true;
        self.command_prefix = log::extend_command_prefix(self.command_prefix, log::command_digest(command)?);
        self.prefix_ring.record(seq, B256::from(self.command_prefix));
        self.commands += 1;
        (self.last_seq, self.last_ts) = (Some(seq), Some(ts));
        let sealer = command.seal.as_ref().map(|seal| seal.signer);
        self.last_sealer = sealer.or(self.last_sealer);
        self.retained.command(Link { seq, link: B256::from(self.command_prefix), ts, sealer });
        Ok(())
    }

    /// Takes a withdrawal entry (see `entry`): a copy's replay takes only a typed authorization, by an attested
    /// signer, of an approval the engine holds when the entry comes.
    fn withdrawal(&mut self, withdrawal: &SignedWithdrawal) -> anyhow::Result<()> {
        let auth = withdrawal.authorization.as_ref().context("withdrawal completion requires a trusted signer")?;
        let signer = self.attested.contains(&auth.signer).then_some(auth.signer);
        crate::checkpoints::signature::verify_withdrawal(withdrawal, Some(self.domain), signer)?;
        let key = (withdrawal.account, withdrawal.nonce);
        let (amount, approved_seq) = self
            .approvals
            .get(&key)
            .filter(|(_, approved_seq)| self.last_seq.is_some_and(|last| *approved_seq <= last))
            .context("typed payout was never approved")?;
        ensure!(
            *amount == withdrawal.amount && Some(*approved_seq) == withdrawal.approved_seq,
            "typed payout differs from its immutable approval"
        );
        ensure!(
            self.payout_checkpoints.get(&auth.checkpoint_seq) == Some(&auth.checkpoint_root),
            "typed payout checkpoint is absent from the preceding log"
        );
        self.signed.insert(key);
        self.authorized.insert(key, withdrawal.clone());
        Ok(())
    }

    /// Takes a checkpoint (see `entry`). Its root is not computed: the seal of the command after it signs a
    /// link that starts from its seq and root.
    fn checkpoint(&mut self, checkpoint: &Checkpoint) -> anyhow::Result<()> {
        self.origin.end_run()?;
        if checkpoint.deferred_withdrawals {
            let deferred =
                self.approved.drain(..).map(|(account, amount, nonce)| (checkpoint.seq, account, amount, nonce));
            self.deferred.extend(deferred);
        } else {
            ensure_signed(self.logs_events, &self.approved, self.last_seq)?;
        }
        if self.last_seq != Some(checkpoint.seq) {
            bail!("checkpoint for seq {} found after seq {:?}", checkpoint.seq, self.last_seq);
        }
        crate::checkpoints::signature::verify_domain(checkpoint, self.domain)?;
        self.requires_checkpoint_gate |= checkpoint.parent.is_some();
        // The pass computes no root, so two checkpoints at one seq must agree on it, as a whole replay's
        // computed root makes them.
        let root = checkpoint.root.parse()?;
        if let Some(held) = self.payout_checkpoints.insert(checkpoint.seq, root) {
            ensure!(held == root, "a second checkpoint at seq {} names another root", checkpoint.seq);
        }
        let signer = checkpoint.signer.parse()?;
        ensure!(
            self.attested.contains(&signer),
            "checkpoint signer {signer} at seq {} has no verified attestation",
            checkpoint.seq
        );
        self.checkpoint_signers.insert(signer);
        self.checkpoints += 1;
        self.origin.checkpoint(checkpoint, self.domain, true)?;
        self.last_signed_checkpoint = Some(checkpoint.clone());
        self.last_checkpoint_offset = self.last_offset;
        self.genesis.checkpoint();
        self.last_checkpoint_ts = self.last_ts;
        self.retained.checkpoint(checkpoint);
        Ok(())
    }

    /// The replica that goes on from the pass: its replay holds `engine`, the snapshot's state, which stands at
    /// the last command the pass took, and what the pass folded, as a whole replay of the copy holds them at
    /// that command, and it retains the checkpoints' custody work from the snapshot's on (`complete` false).
    /// Its next entry must be `checkpoint`, the snapshot's, whose root its replay computes over every key.
    /// Error cases:
    /// - the snapshot stands at another command, time or protocol version than the pass's last command and
    ///   activation, or is not one a recovery cache may stand for (`check_snapshot_provenance`)
    /// - an approval the snapshot holds was made by no command the pass took
    /// - a checkpoint the pass took at the snapshot's seq names another root than `checkpoint`
    pub(crate) fn into_live(self, engine: Engine, checkpoint: &Checkpoint) -> anyhow::Result<LiveReplay> {
        ensure!(
            engine.last_seq() == self.last_seq && Some(engine.now()) == self.last_ts,
            "the snapshot stands at seq {:?}, time {}, not at the copy's command before its checkpoint, seq {:?}",
            engine.last_seq(),
            engine.now(),
            self.last_seq
        );
        ensure!(
            self.version.is_none_or(|version| version == engine.protocol_version()),
            "the snapshot is at protocol {:?}, not {:?}, the copy's last activation before it",
            engine.protocol_version(),
            self.version
        );
        legacy::check_snapshot_provenance(&engine, &self.activations, &self.deferred)?;
        ensure!(
            self.payout_order.len() == engine.withdrawal_approvals().count(),
            "the snapshot holds an approval no command of the copy before it made"
        );
        // A checkpoint at the snapshot's seq before its own, which the replica's replay will not see, must agree
        // with it on the root, as the root a whole replay computes there makes it.
        let root: B256 = checkpoint.root.parse().context("the snapshot's checkpoint root")?;
        ensure!(
            self.payout_checkpoints.get(&checkpoint.seq).is_none_or(|held| *held == root),
            "a second checkpoint at seq {} names another root",
            checkpoint.seq
        );
        // The replica applies the commands after the snapshot's only: it verified no range before them.
        let mut takings = self.takings;
        takings.set_applied_from(engine.last_seq().map_or(0, |seq| seq.saturating_add(1)));
        let replay = Replay {
            origin: self.origin,
            engine,
            commands: self.commands,
            command_prefix: self.command_prefix,
            checkpoints: self.checkpoints,
            last_signed_checkpoint: self.last_signed_checkpoint,
            last_checkpoint_offset: self.last_checkpoint_offset,
            last_offset: self.last_offset,
            params_seq: self.params_seq,
            activations: self.activations,
            approved: self.approved,
            logs_events: self.logs_events,
            genesis: self.genesis,
            last_checkpoint_ts: self.last_checkpoint_ts,
            deferred: self.deferred,
            accounting: self.accounting,
            legacy_deposits: self.legacy_deposits,
            domain: Some(self.domain),
            attested: Some(self.attested),
            checkpoint_signers: self.checkpoint_signers,
            payout_checkpoints: self.payout_checkpoints,
            takings,
            payout_order: self.payout_order,
            signed: self.signed,
            authorized: self.authorized,
            requires_checkpoint_gate: self.requires_checkpoint_gate,
            prefix_ring: self.prefix_ring,
            ..Replay::default()
        };
        let mut live = LiveReplay::wrapping(replay, false);
        live.replay.seed_last_sealer(self.last_sealer);
        live.retained = self.retained;
        Ok(live)
    }
}

/// The protocol version a command activated, given the events it logged: an activation, as the node sequences
/// it, as the operator signs it or as a passed vote applies it, that the engine applied.
fn activated(cmd: &Command, events: &[Event]) -> Option<ProtocolVersion> {
    let admin = match cmd {
        Command::Admin(admin) => admin,
        Command::SignedAdmin(request) => &request.command,
        Command::ApplyProposal(voted) => &voted.command,
        _ => return None,
    };
    let version = match admin {
        engine_core::command::AdminCommand::ActivateProtocol { version }
        | engine_core::command::AdminCommand::ActivateCustodyFinality { version, .. } => *version,
        _ => return None,
    };
    events.contains(&Event::AdminApplied).then_some(version)
}

#[cfg(test)]
mod tests;
