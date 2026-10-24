//! Log replay: re-applies a log as it is read and checks it against what the open-source engine
//! produces.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};
use engine_core::{Engine, Fixed};

use crate::checkpoints::accounting::Accounting;
use crate::log::{self, CheckpointDomain, LogEntry, SegmentedLog};
use crate::sequencer::GenesisProgress;

/// The result of replaying a log.
pub struct Replayed {
    pub engine: Engine,
    pub(crate) accounting: Accounting,
    pub commands: u64,
    pub checkpoints: u64,
    pub(crate) recovery_work: Vec<crate::checkpoints::Work>,
    pub(crate) last_signed_checkpoint: Option<crate::log::Checkpoint>,
    /// Withdrawals the last command approved that the log ends before signing, as a crash between
    /// a command's entry and its withdrawal entry leaves: `(account, amount, nonce)`.
    pub unsigned: Vec<(AccountId, Fixed, u64)>,
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
    pub(crate) payout_proofs: BTreeMap<(AccountId, u64), engine_core::merkle::WithdrawalApprovalProof>,
    pub(crate) payout_order: BTreeMap<(AccountId, u64), usize>,
}

/// Re-applies a log entry by entry, as it is read, and checks every checkpoint root, logged event,
/// and signed withdrawal against what the open-source engine produces. The same code serves crash
/// recovery and third-party audit (`engine replay`).
#[derive(Default)]
pub(super) struct Replay {
    engine: Engine,
    commands: u64,
    checkpoints: u64,
    last_signed_checkpoint: Option<crate::log::Checkpoint>,
    recovery_parent: Option<crate::log::CheckpointParent>,
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
    checkpoint_signers: BTreeSet<AccountId>,
    payout_proofs: BTreeMap<(AccountId, u64), engine_core::merkle::WithdrawalApprovalProof>,
    payout_checkpoints: BTreeMap<u64, engine_core::eip712::B256>,
    authorities: BTreeMap<u64, crate::checkpoints::AcceptedCheckpoint>,
    unproved: BTreeSet<(AccountId, u64)>,
    payout_order: BTreeMap<(AccountId, u64), usize>,
}

impl Replay {
    /// Pins the authority supplied independently by custody, retained key, or audit caller.
    pub(super) fn with_trust(domain: Option<CheckpointDomain>, trusted_signer: Option<AccountId>) -> Self {
        Self { domain, trusted_signer, ..Self::default() }
    }
    /// Retains only the authenticated outstanding tail after the independently finalized anchor.
    pub(super) fn with_recovery(
        domain: Option<CheckpointDomain>,
        trusted_signer: Option<AccountId>,
        parent: crate::log::CheckpointParent,
    ) -> Self {
        Self { recovery_parent: Some(parent), ..Self::with_trust(domain, trusted_signer) }
    }

    /// Pins only independently accepted checkpoints within the finalized recovery anchor.
    /// Error cases:
    /// - an authority lies beyond the trusted prefix, or duplicate identities conflict
    pub(super) fn set_authorities(
        &mut self,
        authorities: Vec<crate::checkpoints::AcceptedCheckpoint>,
    ) -> anyhow::Result<()> {
        for authority in authorities {
            ensure!(
                self.recovery_parent.is_some_and(|parent| parent.has_checkpoint && authority.seq <= parent.seq),
                "historical signer evidence is outside the finalized anchor"
            );
            if let Some(previous) = self.authorities.insert(authority.seq, authority.clone()) {
                ensure!(previous == authority, "conflicting historical signer evidence");
            }
        }
        Ok(())
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
                let events = engine.apply(&c.sequenced).with_context(|| format!("applying seq {seq}"))?;
                self.genesis.command(&c.sequenced, &events);
                if engine.protocol_version() != version {
                    self.activations.push((engine.protocol_version(), seq));
                }
                if c.events.as_ref().is_some_and(|logged| *logged != events) {
                    bail!("events logged at seq {seq} differ from the events replay gives");
                }
                self.accounting.apply(&c.sequenced.cmd, &events)?;
                for (index, event) in events.iter().enumerate() {
                    if let Event::WithdrawalApproved { account, nonce, .. } = event
                        && engine.withdrawal_approval(*account, *nonce).is_some()
                    {
                        self.unproved.insert((*account, *nonce));
                        self.payout_order.insert((*account, *nonce), index);
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
                    self.authorities
                        .get(&auth.checkpoint_seq)
                        .filter(|accepted| {
                            accepted.root == auth.checkpoint_root.to_string()
                                && accepted.signer == auth.signer
                                && accepted.signer_epoch == auth.signer_epoch
                        })
                        .map(|accepted| accepted.signer)
                } else {
                    self.authorities.range(engine.last_seq().unwrap_or(0)..).next().map(|(_, accepted)| accepted.signer)
                };
                let signer = if w.authorization.as_ref().is_some_and(|auth| Some(auth.signer) == self.trusted_signer) {
                    self.trusted_signer
                } else {
                    authority.or(self.trusted_signer)
                };
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
                    return Ok(());
                }
                if let Some(seq) = w.approved_seq {
                    let at = self
                        .deferred
                        .iter()
                        .position(|value| *value == (seq, w.account, w.amount, w.nonce))
                        .context("deferred withdrawal was not approved")?;
                    self.deferred.remove(at);
                    return Ok(());
                }
                if self.approved.front() != Some(&(w.account, w.amount, w.nonce)) {
                    bail!("withdrawal {} nonce {} was not approved at seq {:?}", w.account, w.nonce, engine.last_seq());
                }
                self.approved.pop_front();
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
                crate::checkpoints::signature::verify_with_domain(cp, engine, self.domain)?;
                self.payout_checkpoints.insert(cp.seq, cp.root.parse()?);
                for (account, nonce) in std::mem::take(&mut self.unproved) {
                    self.payout_proofs.insert(
                        (account, nonce),
                        engine.approval_proof(account, nonce).context("checkpoint lacks approval proof")?,
                    );
                }
                let signer = cp.signer.parse()?;
                if self.trusted_signer.is_some_and(|trusted| trusted != signer) {
                    self.authorities
                        .get(&cp.seq)
                        .context("checkpoint signer differs from trusted recovery authority")?
                        .verify(cp)?;
                }
                self.checkpoint_signers.insert(signer);
                self.checkpoints += 1;
                if let Some(parent) = self.recovery_parent
                    && cp.parent.is_some()
                    && (!parent.has_checkpoint || cp.seq > parent.seq)
                {
                    ensure!(cp.parent == Some(parent), "outstanding checkpoint has a divergent signed parent");
                    self.recovery_work.push(crate::checkpoints::Work::capture(cp.clone(), self.accounting));
                    self.recovery_parent = Some(crate::log::CheckpointParent {
                        has_checkpoint: true,
                        seq: cp.seq,
                        root: cp.root.parse()?,
                    });
                }
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
        let unsigned = if self.logs_events { self.approved.into() } else { Vec::new() };
        let (engine, commands, checkpoints, activations, genesis, last_checkpoint_ts) =
            (self.engine, self.commands, self.checkpoints, self.activations, self.genesis, self.last_checkpoint_ts);
        Replayed {
            engine,
            accounting: self.accounting,
            commands,
            checkpoints,
            unsigned,
            activations,
            genesis,
            last_checkpoint_ts,
            deferred: self.deferred,
            unpaid: self.accounting.unpaid(),
            checkpoint_signers: self.checkpoint_signers,
            payout_proofs: self.payout_proofs,
            payout_order: self.payout_order,
            last_signed_checkpoint: self.last_signed_checkpoint,
            recovery_work: self.recovery_work,
        }
    }
}

/// Replays `entries`, a whole log held in memory (see `Replay`).
/// Error cases:
/// - an entry fails replay (see `Replay::apply`)
pub fn replay(entries: &[LogEntry]) -> anyhow::Result<Replayed> {
    let mut replay = Replay::default();
    entries.iter().try_for_each(|entry| replay.apply(entry))?;
    Ok(replay.finish())
}

/// Replays with an independently trusted signer for withdrawal completion evidence.
/// Error cases:
/// - any entry, signature, or authorization fails replay
pub fn replay_with_signer(
    entries: &[LogEntry],
    domain: CheckpointDomain,
    signer: AccountId,
) -> anyhow::Result<Replayed> {
    let mut replay = Replay::with_trust(Some(domain), Some(signer));
    entries.iter().try_for_each(|entry| replay.apply(entry))?;
    let replayed = replay.finish();
    ensure!(
        replayed.checkpoint_signers.iter().all(|named| *named == signer),
        "checkpoint signer differs from trusted signer"
    );
    Ok(replayed)
}

/// Replays the log at `path`, a single log file or a directory of segments with their manifest, as
/// it reads it, for `engine replay`. A log that holds no checkpoint verifies nothing, so it fails.
/// Error cases:
/// - nothing is at `path`, or it cannot be read
/// - the log fails its manifest check (see `SegmentedLog::open`) or replay (see `Replay::apply`)
/// - the log holds no command, or no checkpoint
pub fn replay_path(path: &Path) -> anyhow::Result<Replayed> {
    replay_path_in_domain(path, None)
}

/// Replays signatures in an explicit deployment, including logs that predate domain recording.
/// Error cases:
/// - the log fails replay or conflicts with `domain` (see `replay_path`)
pub fn replay_path_in_domain(path: &Path, domain: Option<CheckpointDomain>) -> anyhow::Result<Replayed> {
    replay_path_with_trust(path, domain, None)
}

/// Replays a published log with explicit authority for authenticating withdrawal releases.
/// Error cases:
/// - log, root, signature, domain or trusted signer validation fails
pub fn replay_path_with_trust(
    path: &Path,
    domain: Option<CheckpointDomain>,
    signer: Option<AccountId>,
) -> anyhow::Result<Replayed> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let mut replay = Replay::with_trust(domain, signer);
    if found.is_dir() {
        SegmentedLog::open(path, |entry| replay.apply(&entry))?;
    } else {
        log::read_file(path, |entry| replay.apply(&entry))?;
    }
    let replayed = replay.finish();
    if let Some(signer) = signer {
        ensure!(
            replayed.checkpoint_signers.iter().all(|named| *named == signer),
            "checkpoint signer differs from trusted signer"
        );
    }
    ensure!(replayed.commands > 0, "{} holds no command", path.display());
    ensure!(replayed.checkpoints > 0, "{} holds no checkpoint, so nothing in it is verified", path.display());
    Ok(replayed)
}

/// Replays one exact finalized prefix with independently authenticated historical authorities.
/// Error cases:
/// - log, root, signature, authority, parent continuity, or finalized anchor validation fails
pub(crate) fn replay_path_with_authorities(
    path: &Path,
    domain: CheckpointDomain,
    signer: AccountId,
    parent: crate::log::CheckpointParent,
    authorities: Vec<crate::checkpoints::AcceptedCheckpoint>,
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
    ensure!(replayed.commands > 0 && replayed.checkpoints > 0, "public log has no verified command/checkpoint");
    Ok(replayed)
}

/// Checks the last command's approved withdrawals all have their signed entries.
/// Error cases:
/// - `must_sign` and an approval is still unmatched
fn ensure_signed(
    must_sign: bool,
    approved: &VecDeque<(AccountId, Fixed, u64)>,
    seq: Option<u64>,
) -> anyhow::Result<()> {
    match approved.front() {
        Some((account, _, nonce)) if must_sign => {
            bail!("withdrawal {account} nonce {nonce} approved at seq {seq:?} has no signed entry")
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
pub(super) mod accounting_tests;
#[cfg(test)]
mod auth_tests;
