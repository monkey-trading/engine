//! Withdrawal signature release only after a custody checkpoint commits its approval.

use std::time::Instant;

use anyhow::Context;
use engine_core::Fixed;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::checkpoints::{LifecycleEvidence, Work};
use crate::log::{Checkpoint, LogEntry};
use crate::sequencer::{Sequencer, SignedWithdrawal, legacy};

/// A recovered approval: its identity, event index, and latest logged `(signer epoch, expiry)`.
pub(super) type BacklogEntry = ((AccountId, u64), usize, Option<(u64, u64)>);

pub(super) struct Gate {
    sink: mpsc::Sender<Work>,
    pub registry: Option<crate::checkpoints::Registry>,
    pub parent: crate::log::CheckpointParent,
    /// The latest collateral reconciliation verdict; `None` before the first observation of this
    /// process (a failed observation leaves the previous verdict). Starts paused (`Pending`)
    /// until the first reconciliation succeeds, and a recovered deployment with posted
    /// checkpoints revalidates collateral before signing again.
    pub health: Option<crate::checkpoints::Health>,
    /// The latest finalized custody evidence shows the vault refusing withdrawal claims: they are
    /// paused, or its signer is retired. Starts clear, as signing waits for a first verdict anyway.
    pub halted: bool,
    /// The latest finalized custody evidence shows a retired signer, so the vault accepts no
    /// checkpoint.
    pub retired: bool,
    pub recovery: Vec<Work>,
    pub pending: Vec<(u64, AccountId, Fixed, u64)>,
    pub payouts: crate::sequencer::payouts::Payouts,
    pub approvals: Option<crate::checkpoints::ApprovalRegistry>,
    pub unproved: std::collections::BTreeSet<(AccountId, u64)>,
    pub unregistered: std::collections::BTreeSet<(AccountId, u64)>,
    pub schedule: crate::sequencer::payout_schedule::Schedule,
    /// Recovered approvals not yet loaded, oldest first, with their event indices and the latest
    /// authorization the log holds for each.
    pub backlog: std::collections::VecDeque<BacklogEntry>,
    /// Loaded recovered approvals that custody has not yet been asked about.
    pub unswept: usize,
    /// Whether a checkpoint is being deferred because the posting queue is full.
    pub backlogged: bool,
    /// The causes (see `Withheld`) whose `checkpoint_withheld` episode is open.
    pub withheld: std::collections::BTreeSet<Withheld>,
    /// Told whether no cause is open after each checkpoint attempt, for a handover to check
    /// before it stops sequencing.
    pub signing: Option<tokio::sync::watch::Sender<bool>>,
}

/// Why signing is withheld. Each is its own alert episode, keyed by the cause alone: the sequence
/// number of the attempt goes in the message, so repeated attempts are one episode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Withheld {
    /// Collateral is unvalidated or diverged.
    Collateral,
    /// The custody prefix awaits the canonical scan.
    CustodyPrefix,
    /// The vault's signer is retired.
    SignerRetired,
}

impl Withheld {
    fn cause(self) -> &'static str {
        match self {
            Withheld::Collateral => "collateral",
            Withheld::CustodyPrefix => "custody_prefix",
            Withheld::SignerRetired => "signer_retired",
        }
    }
}

impl Gate {
    /// Ends the `checkpoint_withheld` episode of `cause`, if open, now that its hold has cleared.
    fn clear_withheld(&mut self, cause: Withheld, seq: u64) {
        let cleared = self.withheld.remove(&cause);
        self.publish_signing();
        if cleared {
            info!(
                alert_resolved = "checkpoint_withheld",
                cause = cause.cause(),
                "checkpoint signing resumed at seq {seq}"
            );
        }
    }

    /// Opens the `checkpoint_withheld` episode of `cause`, if not open, for the caller to alert.
    fn withhold(&mut self, cause: Withheld) {
        self.withheld.insert(cause);
        self.publish_signing();
    }

    /// Tells `signing` whether no cause is open.
    fn publish_signing(&self) {
        if let Some(signing) = &self.signing {
            signing.send_replace(self.withheld.is_empty());
        }
    }

    /// Starts paused until the first reconciliation succeeds.
    pub fn new(sink: mpsc::Sender<Work>, parent: crate::log::CheckpointParent) -> Self {
        Self {
            sink,
            registry: None,
            parent,
            health: (!parent.has_checkpoint).then_some(crate::checkpoints::Health::Pending),
            halted: false,
            retired: false,
            recovery: Vec::new(),
            pending: Vec::new(),
            payouts: Default::default(),
            approvals: None,
            unproved: Default::default(),
            unregistered: Default::default(),
            schedule: Default::default(),
            backlog: Default::default(),
            unswept: 0,
            backlogged: false,
            withheld: Default::default(),
            signing: None,
        }
    }

    /// Whether withdrawal authorizations may be signed: collateral reconciles and the vault is not
    /// halting withdrawal claims.
    pub fn admits_withdrawals(&self) -> bool {
        matches!(self.health, Some(crate::checkpoints::Health::Healthy)) && !self.halted
    }

    /// Why the gate would withhold a checkpoint, for a handover's refusal.
    fn withhold_causes(&self) -> String {
        let mut causes: Vec<&str> = self.withheld.iter().map(|cause| cause.cause()).collect();
        if self.backlogged {
            causes.push("the posting queue is full");
        }
        if causes.is_empty() {
            causes.push("no command has applied");
        }
        causes.join(", ")
    }
}

/// A handover's stop at the checkpoint gate: admitted with a final checkpoint written, or refused
/// with the gate's reason while it would withhold one.
pub(super) enum HandoverGate {
    /// A checkpoint of the last command exists to hand over.
    Admitted,
    /// The gate withholds or defers the final checkpoint; a `HandoverRefused` error, so the node
    /// refuses the handover and goes on sequencing.
    Refused(anyhow::Error),
}

impl Sequencer {
    /// The state root of the engine a test's recovered sequencer holds.
    #[cfg(test)]
    pub fn state_root(&mut self) -> [u8; 32] {
        self.engine.state_root()
    }

    /// Seals the log's open segment for an admitted handover stop: the final checkpoint is durable
    /// once the batch flushed, and the published archive and the successor the keys go to need the
    /// segment it is in. A full live catalog is folded first, as rotation does; a catalog that
    /// still cannot take the segment degrades to the plain manifest, which remains authoritative.
    /// Error cases:
    /// - the tail segment cannot be sealed
    pub(super) fn seal_handover_tail(&mut self) -> anyhow::Result<()> {
        self.log.seal_open_segment().context("sealing the log's final segment for the handover")?;
        Ok(())
    }

    /// Attempts the checkpoint a key handover hands over as the final one. The gate's verdict owns
    /// the stop: refused while it would withhold or defer the checkpoint, so the node refuses the
    /// handover and goes on sequencing.
    /// Error cases:
    /// - the log cannot be written
    pub(super) fn handover_checkpoint(&mut self) -> anyhow::Result<HandoverGate> {
        let Some(seq) = self.engine.last_seq() else {
            return Ok(HandoverGate::Refused(anyhow::anyhow!(crate::sequencer::HandoverRefused::new(
                "no command has applied: there is no checkpoint to hand over".into(),
            ))));
        };
        self.checkpoint()?;
        if self.last_checkpoint == Some(seq) {
            return Ok(HandoverGate::Admitted);
        }
        let causes = self.gate.as_ref().map_or_else(|| "no checkpoint was written".to_string(), Gate::withhold_causes);
        Ok(HandoverGate::Refused(anyhow::anyhow!(crate::sequencer::HandoverRefused::new(format!(
            "the final checkpoint is withheld ({causes}): the handover is refused and this node goes on \
                 sequencing until the checkpoint_withheld alert clears"
        ),))))
    }

    /// Transfers replayed durable work to the worker before admitting live commands. A deployment
    /// recovering posted checkpoints has not yet observed collateral this process: signing waits
    /// for the worker's first reconciliation.
    pub(crate) fn take_recovery_checkpoints(&mut self) -> Vec<Work> {
        self.gate.as_mut().map_or_else(Vec::new, |gate| std::mem::take(&mut gate.recovery))
    }

    /// Logs and publishes a signed state root for the last applied seq. From protocol `V9` the root
    /// rehashes only what changed since the previous checkpoint, and API proofs are taken against it.
    /// Error cases:
    /// - no command has applied, or the log cannot be written
    pub(super) fn checkpoint(&mut self) -> anyhow::Result<()> {
        let seq = self.engine.last_seq().context("checkpoint before any command")?;
        if self.dedup_checkpoint_seq == Some(seq) {
            // A scheduled and an explicit checkpoint can meet at one seq; the earlier entry
            // already commits this state, and two signed roots at one seq break historical
            // signer-authority recovery.
            return Ok(());
        }
        if self.gate.as_ref().is_some_and(|gate| gate.parent.has_checkpoint && gate.parent.seq == seq) {
            return Ok(());
        }
        if let Some(gate) = &mut self.gate {
            // The vault accepts no checkpoint until a signer is installed, and the one signed then
            // carries the whole state, so a long retirement leaves no backlog to post one by one.
            if gate.retired {
                gate.withheld.insert(Withheld::SignerRetired);
                warn!(
                    alert = "checkpoint_withheld",
                    cause = Withheld::SignerRetired.cause(),
                    "no checkpoint signed at seq {seq}: the vault's signer is retired"
                );
                return Ok(());
            }
            gate.clear_withheld(Withheld::SignerRetired, seq);
            if matches!(
                gate.health,
                Some(
                    crate::checkpoints::Health::Healthy
                        | crate::checkpoints::Health::Pending
                        | crate::checkpoints::Health::Unproven
                )
            ) {
                gate.clear_withheld(Withheld::Collateral, seq);
            } else {
                // A signed checkpoint is public: anyone may relay it to the vault, and its root makes
                // distributions claimable. Recovery must revalidate its finalized anchor before
                // publishing any successor; proved divergence also withholds every new signature.
                gate.withhold(Withheld::Collateral);
                warn!(
                    alert = "checkpoint_withheld",
                    cause = Withheld::Collateral.cause(),
                    "no checkpoint signed at seq {seq}: collateral is unvalidated or diverged"
                );
                return Ok(());
            }
        }
        let capturable = Work::can_capture(self.accounting, &self.engine);
        if let Some(gate) = &mut self.gate {
            if capturable {
                gate.clear_withheld(Withheld::CustodyPrefix, seq);
            } else {
                gate.withhold(Withheld::CustodyPrefix);
                warn!(
                    alert = "checkpoint_withheld",
                    cause = Withheld::CustodyPrefix.cause(),
                    "no checkpoint signed at seq {seq}: custody prefix awaits canonical scan"
                );
                return Ok(());
            }
        }
        // The worker finalizes one checkpoint at a time, so a full queue defers the next checkpoint
        // instead of stopping the sequencer; unproved approvals wait for the next one that fits.
        if let Some(gate) = &mut self.gate
            && !gate.sink.is_closed()
            && gate.sink.capacity() == 0
        {
            if !std::mem::replace(&mut gate.backlogged, true) {
                warn!(alert = "checkpoint_backlog", "checkpoint deferred at seq {seq}: the posting queue is full");
            }
            return Ok(());
        }
        let (started, root) = (Instant::now(), self.engine.state_root());
        let (elapsed_us, work) = (started.elapsed().as_micros(), self.engine.last_root_work());
        debug!(seq, elapsed_us, leaves = work.map(|w| w.leaves), nodes = work.map(|w| w.nodes), "state root computed");
        let cp = Checkpoint {
            parent: self.gate.as_ref().map(|gate| gate.parent),
            seq,
            root: format!("0x{}", hex::encode(root)),
            signature: if let Some(gate) = &self.gate {
                self.key.sign_checkpoint_v2(self.chain_id, &self.vault.0, seq, &root, gate.parent)
            } else {
                self.key.sign_checkpoint(self.chain_id, &self.vault.0, seq, &root)
            },
            signer: self.key.address_hex(),
            domain: Some(crate::log::CheckpointDomain { chain_id: self.chain_id, vault: self.vault }),
            deferred_withdrawals: self.gate.is_some(),
        };
        self.record(LogEntry::Checkpoint(cp.clone()))?;
        self.command_link = crate::log::command_seal::checkpoint_link(&cp)?;
        self.capture_payout_proofs();
        if self.gate.is_some() {
            self.log.sync_unpublished()?;
            self.register_payouts()?;
            self.queue_checkpoint(cp.clone())?;
            if let Some(gate) = &mut self.gate {
                gate.parent = crate::log::CheckpointParent { has_checkpoint: true, seq, root: root.into() };
                if std::mem::take(&mut gate.backlogged) {
                    info!(
                        alert_resolved = "checkpoint_backlog",
                        "checkpoint posting queue has room again at seq {seq}"
                    );
                }
            }
        }
        if self.gate.is_some() {
            self.snapshot_work.push(Work::capture(cp.clone(), self.accounting, &self.engine));
        }
        self.snapshot_signers.insert(AccountId(self.key.address()));
        self.progress.genesis.checkpoint();
        self.progress.checkpoints += 1;
        self.store_snapshot(cp)?;
        (self.last_checkpoint, self.dedup_checkpoint_seq) = (Some(seq), Some(seq));
        self.metrics.last_checkpoint_seconds.set(self.last_ts as f64 / 1000.0);
        Ok(())
    }

    /// Queues a durable checkpoint; bounded backpressure stops the node instead of losing work.
    /// Error cases:
    /// - the checkpoint worker is unavailable or its bounded queue is full
    #[tracing::instrument(skip_all, fields(checkpoint_seq = checkpoint.seq))]
    pub(super) fn queue_checkpoint(&self, checkpoint: Checkpoint) -> anyhow::Result<()> {
        let seq = checkpoint.seq;
        let gate = self.gate.as_ref().context("checkpoint gate is absent")?;
        if gate.sink.is_closed() {
            warn!(alert = "checkpoint_post_failed", "checkpoint worker stopped; durable checkpoint awaits restart");
            return Ok(());
        }
        let work = Work::capture(checkpoint, self.accounting, &self.engine);
        if let Some(registry) = &gate.registry {
            registry
                .lock()
                .map_err(|_| anyhow::anyhow!("checkpoint work registry is poisoned"))?
                .insert(work.checkpoint.seq, work.clone());
        }
        gate.sink
            .try_send(work)
            .map_err(|_| anyhow::anyhow!("checkpoint {seq}: checkpoint worker unavailable or queue full"))
    }

    /// Applies one custody observation: whether the vault halts withdrawal claims, which every
    /// release below then respects, the collateral verdict, and the releases `lifecycle` proves.
    /// Without `lifecycle`, the last halt finding stands, as a failed observation leaves the verdict.
    /// Error cases:
    /// - as `checkpoint_status` and `release_payouts`
    pub(super) fn custody_status(
        &mut self,
        posted: Option<u64>,
        health: Option<crate::checkpoints::Health>,
        lifecycle: Option<LifecycleEvidence>,
    ) -> anyhow::Result<()> {
        if let (Some(gate), Some(evidence)) = (&mut self.gate, &lifecycle) {
            gate.halted = evidence.halts_withdrawals();
            gate.retired = evidence.signer_retired();
        }
        self.checkpoint_status(posted, health)?;
        match lifecycle {
            Some(evidence) => self.release_payouts(&evidence),
            None => Ok(()),
        }
    }

    /// Releases only approvals covered by the confirmed prefix, and only after reconciliation.
    /// A `None` verdict — an observation that failed — leaves the gate's verdict unchanged, so an
    /// RPC error cannot erase a proved divergence. An `Unproven` verdict pauses a healthy gate, and
    /// leaves every other verdict (a startup hold, a pending catch-up, a divergence) as it was.
    /// Error cases:
    /// - the log cannot be written
    pub(super) fn checkpoint_status(
        &mut self,
        posted: Option<u64>,
        health: Option<crate::checkpoints::Health>,
    ) -> anyhow::Result<()> {
        let Some(gate) = &mut self.gate else { return Ok(()) };
        use crate::checkpoints::Health;
        match health {
            None | Some(Health::Unvalidated) => {}
            Some(Health::Unproven) => {
                if gate.health == Some(Health::Healthy) {
                    gate.health = health;
                }
            }
            Some(_) => gate.health = health,
        }
        if let (Some(_), Some(posted)) = (health.filter(|health| *health != Health::Unvalidated), posted) {
            self.metrics.withdrawals_verdict_seq.set(i64::try_from(posted).unwrap_or(i64::MAX));
        }
        self.metrics.withdrawals_paused.set(i64::from(!gate.admits_withdrawals()));
        if health != Some(crate::checkpoints::Health::Healthy) {
            return Ok(());
        }
        if let Some(work) = self.snapshot_work.iter().find(|work| Some(work.checkpoint.seq) == posted) {
            self.snapshot_base =
                Some(crate::snapshot::RecoveryBase::Checkpoint { checkpoint: work.checkpoint.clone() });
            self.snapshot_anchor_work = Some(work.clone());
            self.snapshot_work.retain(|work| Some(work.checkpoint.seq) > posted);
        }
        if gate.halted {
            return Ok(());
        }
        let needs_authorization = ProtocolVersion::LATEST.supports_deposit_finality()
            && gate.pending.iter().any(|(_, account, _, nonce)| {
                self.legacy_approvals.contains(*account, *nonce)
                    && !self.legacy_authorizations.contains_key(&(*account, *nonce))
            });
        let authorized = if needs_authorization {
            Some(self.legacy_approvals.authorize(
                &self.engine,
                &self.legacy_deposits,
                &legacy::FinalityProof::default(),
            )?)
        } else {
            None
        };
        let pending = std::mem::take(&mut gate.pending);
        for (seq, account, amount, nonce) in pending {
            let release_seq = self.withdrawal_release_seq(account, nonce, seq, authorized.as_ref())?;
            if release_seq.is_none_or(|required| posted.is_none_or(|posted| posted < required)) {
                if release_seq.is_none() {
                    error!(alert = "legacy_withdrawal_unproven", %account, nonce,
                        "inherited withdrawal lacks canonical finality proof");
                }
                self.gate.as_mut().expect("checkpoint gate exists").pending.push((seq, account, amount, nonce));
                continue;
            }
            let signature = self.key.sign_withdrawal(self.chain_id, &self.vault.0, account, amount, nonce);
            self.record(LogEntry::Withdrawal(SignedWithdrawal {
                account,
                amount,
                nonce,
                chain_id: self.chain_id,
                vault: self.vault,
                signature,
                approved_seq: Some(seq),
                authorization: None,
            }))?;
            self.legacy_approvals.signed(account, amount, nonce);
            self.legacy_authorizations.remove(&(account, nonce));
        }
        Ok(())
    }
}

#[cfg(test)]
mod accounting_tests;
