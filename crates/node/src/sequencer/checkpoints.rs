//! Withdrawal signature release only after a custody checkpoint commits its approval.

use std::time::Instant;

use anyhow::Context;
use engine_core::Fixed;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::checkpoints::Work;
use crate::log::{Checkpoint, LogEntry};
use crate::sequencer::{Sequencer, SignedWithdrawal, legacy};

pub(super) struct Gate {
    sink: mpsc::Sender<Work>,
    pub registry: Option<crate::checkpoints::Registry>,
    pub parent: crate::log::CheckpointParent,
    pub healthy: bool,
    pub recovery: Vec<Work>,
    pub pending: Vec<(u64, AccountId, Fixed, u64)>,
    pub payouts: crate::sequencer::payouts::Payouts,
    pub approvals: Option<crate::checkpoints::ApprovalRegistry>,
    pub unproved: std::collections::BTreeSet<(AccountId, u64)>,
    pub unregistered: std::collections::BTreeSet<(AccountId, u64)>,
    pub schedule: crate::sequencer::payout_schedule::Schedule,
    /// Whether a checkpoint is being deferred because the posting queue is full.
    pub backlogged: bool,
}

impl Gate {
    /// Starts paused until the first reconciliation succeeds.
    pub fn new(sink: mpsc::Sender<Work>, parent: crate::log::CheckpointParent) -> Self {
        Self {
            sink,
            registry: None,
            parent,
            healthy: false,
            recovery: Vec::new(),
            pending: Vec::new(),
            payouts: Default::default(),
            approvals: None,
            unproved: Default::default(),
            unregistered: Default::default(),
            schedule: Default::default(),
            backlogged: false,
        }
    }
}

impl Sequencer {
    /// Transfers replayed durable work to the worker before admitting live commands.
    pub(crate) fn take_recovery_checkpoints(&mut self) -> Vec<Work> {
        self.gate.as_mut().map_or_else(Vec::new, |gate| std::mem::take(&mut gate.recovery))
    }

    /// Logs and publishes a signed state root for the last applied seq. From protocol `V9` the root
    /// rehashes only what changed since the previous checkpoint, and API proofs are taken against it.
    /// Error cases:
    /// - no command has applied, or the log cannot be written
    pub(super) fn checkpoint(&mut self) -> anyhow::Result<()> {
        let seq = self.engine.last_seq().context("checkpoint before any command")?;
        if self.gate.as_ref().is_some_and(|gate| gate.parent.has_checkpoint && gate.parent.seq == seq) {
            return Ok(());
        }
        // The worker finalizes one checkpoint at a time, so a full queue defers the next checkpoint
        // instead of stopping the sequencer; unproved approvals wait for the next one that fits.
        if let Some(gate) = &mut self.gate
            && !gate.sink.is_closed()
            && gate.sink.capacity() == 0
        {
            if !std::mem::replace(&mut gate.backlogged, true) {
                warn!(alert = "checkpoint_backlog", seq, "checkpoint deferred: the posting queue is full");
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
                    info!(alert_resolved = "checkpoint_backlog", seq, "checkpoint posting queue has room again");
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
        self.last_checkpoint = Some(seq);
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

    /// Releases only approvals covered by the confirmed prefix, and only after reconciliation.
    /// Error cases:
    /// - the log cannot be written
    pub(super) fn checkpoint_status(&mut self, posted: Option<u64>, healthy: bool) -> anyhow::Result<()> {
        let Some(gate) = &mut self.gate else { return Ok(()) };
        gate.healthy = healthy;
        self.metrics.withdrawals_paused.set(i64::from(!healthy));
        if !healthy {
            return Ok(());
        }
        if let Some(work) = self.snapshot_work.iter().find(|work| Some(work.checkpoint.seq) == posted) {
            self.snapshot_base =
                Some(crate::snapshot::RecoveryBase::Checkpoint { checkpoint: work.checkpoint.clone() });
            self.snapshot_work.retain(|work| Some(work.checkpoint.seq) > posted);
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
