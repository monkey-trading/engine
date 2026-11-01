//! Log replay: re-applies a log as it is read and checks it against what the open-source engine
//! produces.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use engine_core::deposits::LegacyDeposit;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, MarketId, Millis};
use engine_core::{Engine, Fixed};

use crate::checkpoints::accounting::Accounting;
use crate::log::{self, CheckpointDomain, LogEntry, SegmentedLog};
use crate::sequencer::GenesisProgress;
use crate::snapshot::RecoveryBase;

/// The result of replaying a log.
pub struct Replayed {
    /// Successful legacy EVM credits, derived from verified commands, never a host cursor.
    pub legacy_deposits: BTreeMap<String, LegacyDeposit>,
    pub(crate) legacy_approvals: crate::sequencer::legacy::Approvals,
    pub(crate) origin: crate::sequencer::origin::Origin,
    pub engine: Engine,
    pub(crate) accounting: Accounting,
    pub commands: u64,
    pub(crate) command_prefix: [u8; 32],
    pub checkpoints: u64,
    pub(crate) recovery_work: Vec<crate::checkpoints::Work>,
    pub(crate) recovery_anchor_work: Option<crate::checkpoints::Work>,
    pub(crate) last_signed_checkpoint: Option<crate::log::Checkpoint>,
    pub(crate) recovery_base: Option<RecoveryBase>,
    /// Withdrawals the last command approved that the log ends before signing, as a crash between
    /// a command's entry and its withdrawal entry leaves: `(account, amount, nonce)`.
    pub unsigned: Vec<(AccountId, Fixed, u64)>,
    /// Durable approvals the engine holds that no withdrawal entry has signed. Only a no-gate node
    /// leaves these: a crash can sit between the approval's checkpoint sync and its signature sync.
    pub unreleased: Vec<engine_core::withdrawal::WithdrawalApproval>,
    /// Each protocol version the log activated, with the seq it took effect at.
    pub activations: Vec<(ProtocolVersion, u64)>,
    /// How far the log got through genesis.
    pub genesis: GenesisProgress,
    /// Sequenced time of the command the last checkpoint covers.
    pub last_checkpoint_ts: Option<Millis>,
    /// Approvals whose custody checkpoints precede their signature release.
    pub deferred: Vec<(u64, AccountId, Fixed, u64)>,
    /// Cumulative approved amount; the public log alone cannot establish which nonces custody paid.
    pub unpaid: Fixed,
    /// Named checkpoint signers, for comparison with independently trusted attestations.
    pub checkpoint_signers: BTreeSet<AccountId>,
    /// Every approval with its event index, in FIFO order.
    pub(crate) payout_order: Vec<((AccountId, u64), usize)>,
    /// The latest authorization each signed approval holds, as `(signer epoch, expiry)`. A recovery
    /// from a checkpoint snapshot sees only the suffix after it.
    pub(crate) authorized: BTreeMap<(AccountId, u64), (u64, u64)>,
    pub(crate) requires_checkpoint_gate: bool,
    /// Whether the log ends inside an access-source failure episode.
    pub(crate) access_failed: bool,
    /// The last mark sampled per market, so the recovered sequencer continues change-only
    /// reporting exactly where the replay left off. Empty without a marks sink.
    pub last_marks: BTreeMap<MarketId, Fixed>,
}

/// Re-applies a log entry by entry, as it is read, and checks every checkpoint root, logged event,
/// and signed withdrawal against what the open-source engine produces. The same code serves crash
/// recovery and third-party audit (`engine replay`).
#[derive(Default)]
pub(super) struct Replay {
    legacy_deposits: BTreeMap<String, LegacyDeposit>,
    legacy_approvals: crate::sequencer::legacy::Approvals,
    origin: crate::sequencer::origin::Origin,
    engine: Engine,
    commands: u64,
    command_prefix: [u8; 32],
    checkpoints: u64,
    last_signed_checkpoint: Option<crate::log::Checkpoint>,
    recovery_parent: Option<crate::log::CheckpointParent>,
    recovery_origin: Option<crate::log::CheckpointParent>,
    recovery_anchor: Option<crate::log::Checkpoint>,
    recovery_anchor_work: Option<crate::checkpoints::Work>,
    recovery_work: Vec<crate::checkpoints::Work>,
    activations: Vec<(ProtocolVersion, u64)>,
    /// Withdrawals the last command approved that no withdrawal entry has matched yet. Entries
    /// written before events were logged have no withdrawal entries. Once a command carries its
    /// events, every later one must, and every approval must be signed.
    approved: VecDeque<(AccountId, Fixed, u64)>,
    logs_events: bool,
    genesis: GenesisProgress,
    last_checkpoint_ts: Option<Millis>,
    deferred: Vec<(u64, AccountId, Fixed, u64)>,
    accounting: Accounting,
    domain: Option<CheckpointDomain>,
    trusted_signer: Option<AccountId>,
    /// Independently attested enclave addresses a pure audit accepts per checkpoint and typed
    /// withdrawal, one per signer epoch a log spanning an operator change carries; none in
    /// ordinary recovery, which pins one authority instead.
    attested: Option<BTreeSet<AccountId>>,
    checkpoint_signers: BTreeSet<AccountId>,
    payout_checkpoints: BTreeMap<u64, engine_core::eip712::B256>,
    authorities: BTreeMap<u64, crate::checkpoints::AcceptedCheckpoint>,
    signer_epochs: BTreeMap<u64, AccountId>,
    payout_order: Vec<((AccountId, u64), usize)>,
    requires_checkpoint_gate: bool,
    access_alerts: crate::sequencer::access::AccessAlerts,
    /// Approvals a withdrawal entry signed, for `finish` to report the durable ones it lacks.
    signed: BTreeSet<(AccountId, u64)>,
    /// The latest typed authorization each approval holds in this log, as `(signer epoch, expiry)`.
    authorized: BTreeMap<(AccountId, u64), (u64, u64)>,
    /// The recovery-time marks sink: takes each market's mark as it changes, to seed the derived
    /// marks cache. Nothing without one: `engine replay` does not seed a cache.
    marks: Option<crate::history::marks::MarkSink>,
    /// The last mark taken per market, so only changes reach the sink.
    last_marks: BTreeMap<engine_core::types::MarketId, Fixed>,
}

impl Replay {
    /// Pins the authority supplied independently by custody, retained key, or audit caller.
    pub(super) fn with_trust(domain: Option<CheckpointDomain>, trusted_signer: Option<AccountId>) -> Self {
        Self {
            domain,
            trusted_signer,
            origin: crate::sequencer::origin::Origin::new(domain, trusted_signer),
            ..Self::default()
        }
    }

    /// Accepts the independently attested signers, one per signer epoch, instead of one pinned
    /// authority: each checkpoint, each typed withdrawal, and each command seal must be one of
    /// them, and each authenticated checkpoint covers the commands before it.
    pub(super) fn with_attested(domain: Option<CheckpointDomain>, attested: BTreeSet<AccountId>) -> Self {
        let origin = crate::sequencer::origin::Origin::with_attested(domain, attested.clone());
        Self { attested: Some(attested), domain: domain, trusted_signer: None, origin, ..Self::default() }
    }
    /// Retains only the authenticated outstanding tail after the independently finalized anchor.
    pub(super) fn with_recovery(
        domain: Option<CheckpointDomain>,
        trusted_signer: Option<AccountId>,
        parent: crate::log::CheckpointParent,
    ) -> Self {
        Self {
            recovery_parent: Some(parent),
            recovery_origin: Some(parent),
            ..Self::with_trust(domain, trusted_signer)
        }
    }
    /// Applies the log's next entry.
    /// Error cases:
    /// - a command out of sequence, with time running backwards, or stamped past the range the
    ///   engine accepts
    /// - logged events that differ from the replayed ones, or a command without logged events after
    ///   one with them
    /// - a withdrawal entry that does not match the next unmatched withdrawal its command approved
    /// - a withdrawal approved by a command logged with its events, with no withdrawal entry before
    ///   the next command or checkpoint
    /// - a checkpoint for a seq other than the last applied
    /// - a checkpoint root that differs from the replayed state
    pub(super) fn apply(&mut self, entry: &LogEntry) -> anyhow::Result<()> {
        let engine = &mut self.engine;
        match entry {
            LogEntry::Command(c) => {
                ensure_signed(self.logs_events, &self.approved, engine.last_seq())?;
                let seq = c.sequenced.seq;
                if self.logs_events && c.events.is_none() {
                    bail!("seq {seq} has no logged events, though earlier commands do");
                }
                let version = engine.protocol_version();
                self.origin.command(c, version)?;
                crate::sequencer::legacy::check_activation_ledger(&c.sequenced.cmd, &self.legacy_deposits)?;
                let events = engine.apply(&c.sequenced).with_context(|| format!("applying seq {seq}"))?;
                self.genesis.command(&c.sequenced, &events);
                if engine.protocol_version() != version {
                    self.activations.push((engine.protocol_version(), seq));
                }
                if c.events.as_ref().is_some_and(|logged| *logged != events) {
                    bail!("events logged at seq {seq} differ from the events replay gives");
                }
                marks::sample_marks(&mut self.marks, &mut self.last_marks, engine, c.sequenced.ts);
                self.accounting.apply(&c.sequenced.cmd, &events)?;
                if !version.supports_deposit_finality()
                    && let Some(deposit) = LegacyDeposit::from_command(&c.sequenced.cmd, &events)
                {
                    self.legacy_deposits.insert(deposit.chain_ref.clone(), deposit);
                }
                self.legacy_approvals.observe(engine, &c.sequenced, &events, c.events.is_some());
                self.access_alerts.observe(engine, &c.sequenced.cmd, &events);
                for (index, event) in events.iter().enumerate() {
                    if let Event::WithdrawalApproved { account, nonce, .. } = event
                        && engine.withdrawal_approval(*account, *nonce).is_some()
                    {
                        self.payout_order.push(((*account, *nonce), index));
                    }
                }
                self.approved = events
                    .into_iter()
                    .filter_map(|e| match e {
                        Event::WithdrawalApproved { account, amount, nonce }
                            if engine.withdrawal_approval(account, nonce).is_none() =>
                        {
                            Some((account, amount, nonce))
                        }
                        _ => None,
                    })
                    .collect();
                self.logs_events = c.events.is_some();
                self.command_prefix =
                    crate::log::extend_command_prefix(self.command_prefix, crate::log::command_digest(c)?);
                self.commands += 1;
            }
            LogEntry::Withdrawal(w) => {
                let recorded = engine
                    .operator()
                    .map(|operator| CheckpointDomain { chain_id: operator.chain_id, vault: operator.vault })
                    .or_else(|| match engine.client_auth() {
                        Some(engine_core::auth::ClientAuth::Eip712 { chain_id, vault }) => {
                            Some(CheckpointDomain { chain_id: *chain_id, vault: *vault })
                        }
                        _ => None,
                    });
                if let (Some(explicit), Some(recorded)) = (self.domain, recorded) {
                    ensure!(explicit == recorded, "withdrawal trusted deployment differs from recorded deployment");
                }
                let authority = if let Some(auth) = &w.authorization {
                    if self.signer_epochs.is_empty() {
                        self.trusted_signer
                    } else {
                        ensure!(
                            self.authorities
                                .get(&auth.checkpoint_seq)
                                .is_some_and(|accepted| accepted.root == auth.checkpoint_root.to_string()),
                            "typed payout root was not accepted in the finalized prefix"
                        );
                        let signer = self
                            .signer_epochs
                            .get(&auth.signer_epoch)
                            .context("typed payout signer epoch lacks authenticated history")?;
                        ensure!(*signer == auth.signer, "typed payout signer differs from immutable epoch history");
                        Some(*signer)
                    }
                } else {
                    self.authorities.range(engine.last_seq().unwrap_or(0)..).next().map(|(_, accepted)| accepted.signer)
                };
                let signer = authority.or(self.trusted_signer).or_else(|| {
                    // A pure multi-signer audit has no single pinned authority: a typed withdrawal
                    // authenticates against its own authorization's signer, which the attested set
                    // must name.
                    let auth = w.authorization.as_ref()?;
                    self.attested.as_ref()?.contains(&auth.signer).then_some(auth.signer)
                });
                crate::checkpoints::signature::verify_withdrawal(w, self.domain.or(recorded), signer)?;
                if let Some(auth) = &w.authorization {
                    let approval =
                        engine.withdrawal_approval(w.account, w.nonce).context("typed payout was never approved")?;
                    ensure!(
                        approval.amount == w.amount && Some(approval.approved_seq) == w.approved_seq,
                        "typed payout differs from its immutable approval"
                    );
                    ensure!(
                        self.payout_checkpoints.get(&auth.checkpoint_seq) == Some(&auth.checkpoint_root),
                        "typed payout checkpoint is absent from the preceding log"
                    );
                    self.signed.insert((w.account, w.nonce));
                    self.authorized.insert((w.account, w.nonce), (auth.signer_epoch, auth.expires_at));
                    return Ok(());
                }
                if let Some(seq) = w.approved_seq {
                    let at = self
                        .deferred
                        .iter()
                        .position(|value| *value == (seq, w.account, w.amount, w.nonce))
                        .context("deferred withdrawal was not approved")?;
                    self.deferred.remove(at);
                    self.legacy_approvals.signed(w.account, w.amount, w.nonce);
                    self.signed.insert((w.account, w.nonce));
                    return Ok(());
                }
                if self.approved.front() != Some(&(w.account, w.amount, w.nonce)) {
                    bail!("withdrawal {} nonce {} was not approved at seq {:?}", w.account, w.nonce, engine.last_seq());
                }
                self.approved.pop_front();
                self.legacy_approvals.signed(w.account, w.amount, w.nonce);
                self.signed.insert((w.account, w.nonce));
            }
            LogEntry::Checkpoint(cp) => {
                if cp.deferred_withdrawals {
                    self.deferred.extend(
                        self.approved.drain(..).map(|(account, amount, nonce)| (cp.seq, account, amount, nonce)),
                    );
                } else {
                    ensure_signed(self.logs_events, &self.approved, engine.last_seq())?;
                }
                if engine.last_seq() != Some(cp.seq) {
                    bail!("checkpoint for seq {} found after seq {:?}", cp.seq, engine.last_seq());
                }
                let root = format!("0x{}", hex::encode(engine.state_root()));
                if root != cp.root {
                    bail!("state root mismatch at seq {}: log says {}, replay gives {root}", cp.seq, cp.root);
                }
                let checkpoint_domain = crate::checkpoints::signature::verify_with_domain(cp, engine, self.domain)?;
                self.requires_checkpoint_gate |= cp.parent.is_some();
                self.payout_checkpoints.insert(cp.seq, cp.root.parse()?);
                let signer = cp.signer.parse()?;
                if let Some(attested) = &self.attested {
                    ensure!(
                        attested.contains(&signer),
                        "checkpoint signer {signer} at seq {} has no verified attestation",
                        cp.seq
                    );
                }
                if self.trusted_signer.is_some_and(|trusted| trusted != signer) {
                    if let Some(accepted) = self.authorities.get(&cp.seq) {
                        accepted.verify(cp)?;
                    } else {
                        // A checkpoint custody has not authenticated: the outgoing operator's
                        // last published checkpoints past the vault's latest. Accept only the
                        // linear extension of the last authenticated checkpoint, signed by the
                        // same signer: the signature above binds the root, the parent binds the
                        // chain, and a key the vault never named cannot pass.
                        let previous = self
                            .authorities
                            .range(..cp.seq)
                            .next_back()
                            .map(|(_, accepted)| accepted)
                            .context("checkpoint signer differs from trusted recovery authority")?;
                        let parent = cp.parent.context("unauthenticated checkpoint lacks its signed parent")?;
                        ensure!(
                            previous.signer == signer
                                && parent.has_checkpoint
                                && parent.seq == previous.seq
                                && parent.root.to_string() == previous.root,
                            "checkpoint signer differs from trusted recovery authority"
                        );
                        self.authorities.insert(
                            cp.seq,
                            crate::checkpoints::AcceptedCheckpoint {
                                seq: cp.seq,
                                root: cp.root.clone(),
                                signer,
                                signer_epoch: previous.signer_epoch,
                            },
                        );
                    }
                }
                self.checkpoint_signers.insert(signer);
                if self.recovery_origin.is_some_and(|parent| {
                    parent.has_checkpoint && parent.seq == cp.seq && parent.root.to_string() == cp.root
                }) {
                    self.recovery_anchor = Some(cp.clone());
                    self.recovery_anchor_work =
                        Some(crate::checkpoints::Work::capture(cp.clone(), self.accounting, engine));
                }
                self.checkpoints += 1;
                if let Some(parent) = self.recovery_parent
                    && cp.parent.is_some()
                    && (!parent.has_checkpoint || cp.seq > parent.seq)
                {
                    ensure!(cp.parent == Some(parent), "outstanding checkpoint has a divergent signed parent");
                    // Only the pinned authority's outstanding checkpoints queue as posting work:
                    // a takeover keeps the outgoing operator's later published checkpoints as
                    // replayed history, but the vault accepts only the current signer's posts.
                    if self.trusted_signer.is_none_or(|trusted| trusted == signer) {
                        self.recovery_work.push(crate::checkpoints::Work::capture(cp.clone(), self.accounting, engine));
                    }
                    self.recovery_parent = Some(crate::log::CheckpointParent {
                        has_checkpoint: true,
                        seq: cp.seq,
                        root: cp.root.parse()?,
                    });
                }
                self.origin.checkpoint(
                    cp,
                    checkpoint_domain,
                    self.trusted_signer.is_some() || self.attested.is_some(),
                )?;
                self.last_signed_checkpoint = Some(cp.clone());
                self.genesis.checkpoint();
                // The checkpoint covers the last command applied, whose stamp is the engine's time.
                self.last_checkpoint_ts = Some(engine.now());
            }
        }
        Ok(())
    }

    /// What the entries applied so far replay to.
    pub(super) fn finish(self) -> Replayed {
        let recovery_base = match self.recovery_origin {
            None => Some(RecoveryBase::Ungated),
            Some(parent) if !parent.has_checkpoint => Some(RecoveryBase::Genesis),
            Some(_) => self.recovery_anchor.clone().map(|checkpoint| RecoveryBase::Checkpoint { checkpoint }),
        };
        let unsigned = if self.logs_events { self.approved.into() } else { Vec::new() };
        let unreleased = self
            .engine
            .withdrawal_approvals()
            .filter(|approval| !self.signed.contains(&(approval.account, approval.nonce)))
            .copied()
            .collect();
        let (engine, commands, checkpoints, activations, genesis, last_checkpoint_ts) =
            (self.engine, self.commands, self.checkpoints, self.activations, self.genesis, self.last_checkpoint_ts);
        Replayed {
            origin: self.origin,
            engine,
            command_prefix: self.command_prefix,
            accounting: self.accounting,
            commands,
            checkpoints,
            unsigned,
            unreleased,
            activations,
            genesis,
            last_checkpoint_ts,
            deferred: self.deferred,
            unpaid: self.accounting.unpaid(),
            checkpoint_signers: self.checkpoint_signers,
            payout_order: self.payout_order,
            authorized: self.authorized,
            requires_checkpoint_gate: self.requires_checkpoint_gate,
            access_failed: self.access_alerts.failed(),
            last_marks: self.last_marks,
            last_signed_checkpoint: self.last_signed_checkpoint,
            recovery_work: self.recovery_work,
            recovery_anchor_work: self.recovery_anchor_work,
            recovery_base,
            legacy_deposits: self.legacy_deposits,
            legacy_approvals: self.legacy_approvals,
        }
    }
}

pub use paths::{
    replay_path, replay_path_in_domain, replay_path_with_attested, replay_path_with_trust, replay_with_signer,
};

/// Replays `entries`, a whole log held in memory (see `Replay`).
/// Error cases:
/// - an entry fails replay (see `Replay::apply`)
pub fn replay(entries: &[LogEntry]) -> anyhow::Result<Replayed> {
    let mut replay = Replay::default();
    entries.iter().try_for_each(|entry| replay.apply(entry))?;
    Ok(replay.finish())
}

/// Replays one exact finalized prefix with independently authenticated historical authorities.
/// Error cases:
/// - log, root, signature, authority, parent continuity, or finalized anchor validation fails
pub(crate) fn replay_path_with_authorities(
    path: &Path,
    domain: CheckpointDomain,
    signer: AccountId,
    parent: crate::log::CheckpointParent,
    authorities: crate::checkpoints::SignerHistory,
) -> anyhow::Result<Replayed> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let mut replay = Replay::with_recovery(Some(domain), Some(signer), parent);
    replay.set_authorities(authorities)?;
    let mut anchored = !parent.has_checkpoint;
    let mut visit = |entry: LogEntry| {
        if let LogEntry::Checkpoint(cp) = &entry
            && parent.has_checkpoint
            && cp.seq == parent.seq
        {
            ensure!(
                cp.root.parse::<engine_core::eip712::B256>()? == parent.root,
                "public log differs from the finalized vault anchor"
            );
            anchored = true;
        }
        replay.apply(&entry)
    };
    if found.is_dir() {
        SegmentedLog::open(path, &mut visit)?;
    } else {
        log::read_file(path, &mut visit)?;
    }
    ensure!(anchored, "public log is shorter than or omits the finalized vault anchor");
    let replayed = replay.finish();
    replayed.origin.require_recovery(domain, signer)?;
    ensure!(replayed.commands > 0 && replayed.checkpoints > 0, "public log has no verified command/checkpoint");
    Ok(replayed)
}

#[cfg(test)]
pub(super) mod accounting_tests;
#[cfg(test)]
mod auth_tests;
mod authorities;
mod marks;
mod paths;
mod signed;
mod snapshot;
use signed::ensure_signed;

#[cfg(test)]
mod origin_tests;
