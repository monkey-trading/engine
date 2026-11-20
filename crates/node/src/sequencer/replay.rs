//! Log replay: re-applies a log as it is read and checks it against what the open-source engine
//! produces.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Context, bail, ensure};
use engine_core::deposits::LegacyDeposit;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};
use engine_core::{Engine, Fixed};

use crate::checkpoints::Work;
use crate::checkpoints::accounting::Accounting;
use crate::history::derived::Derived;
use crate::log::{self, CheckpointDomain, LogEntry};
use crate::sequencer::{GenesisProgress, SignedWithdrawal};
use crate::snapshot::RecoveryBase;

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
    last_checkpoint_offset: Option<log::Offset>,
    /// The offset of the entry applied last, from which the next one's follows (see `log::offset_of`).
    last_offset: Option<log::Offset>,
    params_seq: Option<u64>,
    recovery_parent: Option<crate::log::CheckpointParent>,
    recovery_origin: Option<crate::log::CheckpointParent>,
    recovery_anchor: Option<crate::log::Checkpoint>,
    /// The sequenced time of the command the vault-finalized checkpoint covers.
    recovery_anchor_ts: Option<Millis>,
    recovery_anchor_work: Option<Work>,
    recovery_work: Vec<Work>,
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
    /// The roots of the checkpoints a typed authorization may name: every one the log holds, and, from a
    /// cache, those it carries and its own.
    payout_checkpoints: BTreeMap<u64, engine_core::eip712::B256>,
    takings: crate::log_check::Takings,
    authorities: BTreeMap<u64, crate::checkpoints::AcceptedCheckpoint>,
    signer_epochs: BTreeMap<u64, AccountId>,
    /// What a promotion's copy needs noted as it replays (see `unheld`).
    unheld: unheld::Noted,
    payout_order: Vec<((AccountId, u64), usize)>,
    /// Approvals a cache's node had delivered before its checkpoint, which `payout_order` leaves out.
    delivered: usize,
    requires_checkpoint_gate: bool,
    access_alerts: crate::sequencer::access::AccessAlerts,
    /// Approvals a withdrawal entry signed, for `finish` to report the durable ones it lacks.
    signed: BTreeSet<(AccountId, u64)>,
    /// The latest typed authorization each approval holds in this log; from a cache, only of the
    /// approvals it still owed and those signed after it.
    authorized: BTreeMap<(AccountId, u64), SignedWithdrawal>,
    /// The recovery-time marks sink: takes each market's mark as it changes, to seed the derived
    /// marks cache. Nothing without one: `engine replay` does not seed a cache.
    marks: Option<crate::history::marks::MarkSink>,
    /// The last mark taken per market, so only changes reach the sink.
    last_marks: BTreeMap<engine_core::types::MarketId, Fixed>,
    /// The dashboard record recovery seeds: it takes every command this replay applies.
    stats: Option<crate::stats::Stats>,
    /// When set, the command arm records the cumulative command-prefix digest and the command's
    /// sequenced time at this seq for the posted-head check.
    capture_link_at: Option<u64>,
    captured_link: Option<engine_core::eip712::B256>,
    captured_ts: Option<Millis>,
    /// The last `crate::log::PrefixRing::CAPACITY` (seq, cumulative-prefix) pairs: the poster's
    /// adoption evidence, seeded before its first tick.
    prefix_ring: crate::log::PrefixRing,
    /// The newest commands' derived rows, which the sequencer keeps from where the replay ends.
    derived: crate::history::derived::DerivedRows,
    /// Where the state roots are computed: in the engine, or on a thread of their own.
    roots: roots::Roots,
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
        Self { attested: Some(attested), domain, trusted_signer: None, origin, ..Self::default() }
    }

    /// `with_attested` in `domain`, for a copy of another node's log: every command must also carry a
    /// seal, by one of `attested` once a checkpoint covers it; a tail no checkpoint covers is the caller's
    /// to refuse or drop (`Origin::copy`).
    pub(super) fn copy(domain: CheckpointDomain, attested: BTreeSet<AccountId>) -> Self {
        let origin = crate::sequencer::origin::Origin::copy(domain, attested.clone());
        Self { origin, ..Self::with_attested(Some(domain), attested) }
    }
    /// Adds `signer` to the attested signers, for checkpoints, typed withdrawals and command seals
    /// alike: a key the vault named after the set was read (#595's promotion), which the caller
    /// authenticated. No effect on a replay pinned to one authority.
    pub(crate) fn admit(&mut self, signer: AccountId) {
        if let Some(attested) = self.attested.as_mut() {
            attested.insert(signer);
            self.origin.admit(signer);
        }
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

    /// Records the cumulative command-prefix digest at `seq` as the replay passes its command: the
    /// posted-head check binds to the replayed bytes' own prefix, never a second read of host files.
    pub(super) fn capture_link_at(&mut self, seq: u64) {
        self.capture_link_at = Some(seq);
    }

    /// The derived rows of the command at `seq`, while the replay keeps them (`DerivedRows`).
    pub(super) fn derived_at(&self, seq: u64) -> Option<crate::history::derived::Rows> {
        self.derived.get(seq).map(|derived| derived.rows.clone())
    }

    /// Has the seals of the log at `path` checked ahead of this replay by `workers`, from the next
    /// command it applies until it finishes (`Origin::check_ahead`).
    pub(super) fn check_seals_ahead(&mut self, path: &std::path::Path, workers: crate::log::ReplayWorkers) {
        let from = self.engine.last_seq().map_or(0, |seq| seq + 1);
        self.origin.check_ahead(path, from, workers);
    }

    /// Applies the log's next entry, the work of `apply`.
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
    fn apply_entry(&mut self, entry: &LogEntry) -> anyhow::Result<()> {
        self.before(entry)?;
        let engine = &mut self.engine;
        self.last_offset = log::offset_of(self.last_offset, entry).ok();
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
                self.roots.after_command(engine)?;
                self.genesis.command(&c.sequenced, &events);
                if crate::sequencer::house::applied_set_params(&c.sequenced.cmd, &events) {
                    self.params_seq = Some(seq);
                }
                if engine.protocol_version() != version {
                    self.activations.push((engine.protocol_version(), seq));
                }
                if c.events.as_ref().is_some_and(|logged| *logged != events) {
                    bail!("events logged at seq {seq} differ from the events replay gives");
                }
                if let Some(derived) = Derived::of(seq, &events, engine.take_derived())? {
                    self.derived.keep(seq, derived);
                }
                marks::sample_marks(&mut self.marks, &mut self.last_marks, engine, c.sequenced.ts);
                if let Some(stats) = &self.stats {
                    stats.observe(engine, c.sequenced.ts, &events);
                }
                self.accounting.apply(&c.sequenced.cmd, &events)?;
                if !version.supports_deposit_finality()
                    && let Some(deposit) = LegacyDeposit::from_command(&c.sequenced.cmd, &events)
                {
                    self.legacy_deposits.insert(deposit.chain_ref.clone(), deposit);
                }
                self.legacy_approvals.observe(engine, &c.sequenced, &events, c.events.is_some());
                self.access_alerts.observe(engine, &c.sequenced.cmd, &events);
                self.takings.stamped(seq, c.sequenced.ts);
                self.takings.command(seq, &events);
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
                self.prefix_ring.record(seq, engine_core::eip712::B256::from(self.command_prefix));
                if self.capture_link_at == Some(seq) {
                    self.captured_link = Some(engine_core::eip712::B256::from(self.command_prefix));
                    self.captured_ts = Some(c.sequenced.ts);
                }
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
                    self.authorized.insert((w.account, w.nonce), w.clone());
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
                // The run of seals before the checkpoint is checked first: it is the earlier fault.
                self.origin.end_run()?;
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
                self.roots.checkpoint(engine, cp.seq, &cp.root)?;
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
                if self.trusted_signer.is_some_and(|trusted| trusted != signer)
                    && self.superseded_checkpoint(cp, signer, checkpoint_domain)?
                {
                    return Ok(());
                }
                self.checkpoint_signers.insert(signer);
                if self.recovery_origin.is_some_and(|parent| {
                    parent.has_checkpoint && parent.seq == cp.seq && parent.root.to_string() == cp.root
                }) {
                    self.recovery_anchor = Some(cp.clone());
                    self.recovery_anchor_ts = Some(self.engine.now());
                    self.recovery_anchor_work = Some(Work::capture(cp.clone(), self.accounting, &self.engine));
                }
                self.checkpoints += 1;
                if let Some(parent) = self.recovery_parent
                    && cp.parent.is_some()
                    && (!parent.has_checkpoint || cp.seq > parent.seq)
                {
                    ensure!(cp.parent == Some(parent), "outstanding checkpoint has a divergent signed parent");
                    self.recovery_work.push(Work::capture(cp.clone(), self.accounting, &self.engine));
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
                self.last_checkpoint_offset = self.last_offset;
                self.genesis.checkpoint();
                // The checkpoint covers the last command applied, whose stamp is the engine's time.
                self.last_checkpoint_ts = Some(self.engine.now());
            }
        }
        Ok(())
    }

    /// What the entries applied so far replay to.
    pub(super) fn finish(mut self) -> Replayed {
        self.origin.stop_checking_ahead();
        // A replay settles its read (`settled`) before it finishes, taking its tree back and ending its run.
        assert!(!self.roots.on_thread(), "a replay finishes with its engine holding its tree");
        assert!(!self.origin.run_open(), "a replay finishes with its last run of seals checked");
        self.anchor_unheld().expect("an engine holding its tree computes its root in place");
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
            delivered: self.delivered,
            authorized: self.authorized,
            requires_checkpoint_gate: self.requires_checkpoint_gate,
            access_failed: self.access_alerts.failed(),
            last_marks: self.last_marks,
            last_signed_checkpoint: self.last_signed_checkpoint,
            last_checkpoint_offset: self.last_checkpoint_offset,
            params_seq: self.params_seq,
            recovery_work: self.recovery_work,
            recovery_anchor_work: self.recovery_anchor_work,
            recovery_base,
            legacy_deposits: self.legacy_deposits,
            legacy_approvals: self.legacy_approvals,
            link_at: self.captured_link,
            ts_at: self.captured_ts,
            anchor_ts: self.recovery_anchor_ts,
            prefix_ring: self.prefix_ring,
            derived: self.derived,
            takings: self.takings,
            checkpoint_roots: self.payout_checkpoints,
        }
    }
}

#[cfg(test)]
pub(crate) use paths::replay_path_from;
pub(crate) use paths::{replay_copy, replay_path_with_authorities};
pub use paths::{
    replay_path, replay_path_in_domain, replay_path_with_attested, replay_path_with_trust, replay_with_signer,
};

/// Replays `entries`, a whole log held in memory (see `Replay`), its command seals checked at the end of
/// each run (`SealChecks::RunEnds`).
/// Error cases:
/// - an entry fails replay (see `Replay::apply`)
pub fn replay(entries: &[LogEntry]) -> anyhow::Result<Replayed> {
    let mut replay = Replay::default();
    replay.check_run_ends();
    replay.check_roots_ahead();
    let applied = entries.iter().try_for_each(|entry| replay.apply(entry));
    replay.settled(applied)?;
    Ok(replay.finish())
}

#[cfg(test)]
pub(super) mod accounting_tests;
#[cfg(test)]
mod ahead_tests;
#[cfg(test)]
mod auth_tests;
mod authorities;
mod live;
pub(crate) use live::LiveReplay;
mod marks;
mod paths;
mod replayed;
pub use replayed::Replayed;
mod replayed_log;
pub use replayed_log::ReplayedLog;
mod roots;
mod signed;
mod snapshot;
mod stats;
mod unheld;
use signed::ensure_signed;

#[cfg(test)]
mod origin_tests;
#[cfg(test)]
mod run_ends_tests;
