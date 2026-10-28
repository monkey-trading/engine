//! Delivery state derived from durable approvals and authenticated finalized custody evidence.

use std::collections::BTreeMap;

use anyhow::{Context, ensure};
use engine_core::merkle::WithdrawalApprovalProof;
use engine_core::types::AccountId;
use engine_core::withdrawal::WithdrawalApproval;
use tracing::info;

use crate::checkpoints::{APPROVAL_PAGE_SIZE, ApprovalId, LifecycleEvidence};
use crate::log::LogEntry;
use crate::sequencer::{Sequencer, SignedWithdrawal};
use crate::withdrawals::{Authorization, LIFETIME_SECONDS};

/// A signature is delivery progress; the contract's used nonce is the payment authority.
pub(super) struct Payout {
    pub approval: WithdrawalApproval,
    pub order: (u64, usize),
    pub proof: Option<WithdrawalApprovalProof>,
    pub released: Option<(u64, u64)>,
    pub observed_epoch: Option<u64>,
    /// Loaded from the recovery backlog and not yet observed at a finalized block.
    pub recovered: bool,
}

impl Payout {
    /// Starts an unpaid delivery reconstructed from the deterministic approval record.
    pub fn new(approval: WithdrawalApproval, event_index: usize) -> Self {
        Self {
            order: (approval.approved_seq, event_index),
            approval,
            proof: None,
            released: None,
            observed_epoch: None,
            recovered: false,
        }
    }
}

/// Recovered approvals loaded, and so proved and observed, at a time.
pub(super) const BACKLOG_WINDOW: usize = 4 * APPROVAL_PAGE_SIZE;

impl Sequencer {
    /// Loads the oldest recovered approvals once the previous window was observed, so a restart
    /// holds proofs and queries custody only for a window plus the approvals still unpaid. An
    /// unobserved loaded head blocks every release, so no newer approval passes an unloaded one.
    /// Error cases:
    /// - the backlog names an approval the engine lacks, or the registry is poisoned
    pub(super) fn load_backlog(&mut self) -> anyhow::Result<()> {
        let Some(gate) = &mut self.gate else { return Ok(()) };
        if gate.unswept > 0 {
            return Ok(());
        }
        for _ in 0..BACKLOG_WINDOW {
            let Some((key, index)) = gate.backlog.pop_front() else { break };
            let approval = *self
                .engine
                .withdrawal_approval(key.0, key.1)
                .context("immutable approval lacks its validated event order")?;
            let mut payout = Payout::new(approval, index);
            payout.recovered = true;
            gate.schedule.insert(payout.order, key);
            gate.payouts.insert(key, payout);
            gate.unproved.insert(key);
            gate.unregistered.insert(key);
            gate.unswept += 1;
        }
        self.register_payouts()
    }

    /// Proves loaded approvals against the checkpoint that already covers the engine state, or
    /// writes one that does.
    /// Error cases:
    /// - the checkpoint cannot be written
    pub(super) fn prove_loaded(&mut self) -> anyhow::Result<()> {
        let Some(gate) = &self.gate else { return Ok(()) };
        if gate.unproved.is_empty() {
            return Ok(());
        }
        if gate.parent.has_checkpoint && Some(gate.parent.seq) == self.engine.last_seq() {
            self.capture_payout_proofs();
            Ok(())
        } else {
            self.checkpoint()
        }
    }

    /// Captures each pending approval's first proof, before another command changes the tree.
    pub(super) fn capture_payout_proofs(&mut self) {
        let Some(gate) = &mut self.gate else { return };
        for key in std::mem::take(&mut gate.unproved) {
            let payout = gate.payouts.get_mut(&key).expect("an unproved payout is pending");
            let approval = payout.approval;
            payout.proof = self.engine.approval_proof(approval.account, approval.nonce);
            assert!(payout.proof.is_some(), "a checkpoint includes every preceding approval");
        }
    }

    /// Exposes only synced approvals to the checkpoint worker's bounded nonce-use queries.
    /// Error cases:
    /// - the worker registry is poisoned or an approval identity conflicts
    pub(super) fn register_payouts(&mut self) -> anyhow::Result<()> {
        let Some(gate) = &mut self.gate else { return Ok(()) };
        let Some(registry) = &gate.approvals else { return Ok(()) };
        let mut registry = registry.lock().map_err(|_| anyhow::anyhow!("approval registry is poisoned"))?;
        for key in std::mem::take(&mut gate.unregistered) {
            let payout = gate.payouts.get(&key).expect("an unregistered payout is pending");
            let approval = payout.approval;
            let id =
                ApprovalId { account: approval.account, nonce: approval.nonce, approved_seq: approval.approved_seq };
            if let Some(previous) = registry.insert(key, id) {
                ensure!(previous == id, "conflicting immutable withdrawal identity");
            }
        }
        Ok(())
    }

    /// Releases or reissues only the identities explicitly observed unpaid at a finalized block.
    /// Error cases:
    /// - custody evidence conflicts, the approval proof is invalid, or the log cannot be written
    pub(super) fn release_payouts(&mut self, evidence: &LifecycleEvidence) -> anyhow::Result<()> {
        ensure!(evidence.signer == AccountId(self.key.address()), "payout signer differs from this enclave");
        let Some(gate) = &mut self.gate else { return Ok(()) };
        for observed in &evidence.approvals {
            let key = (observed.approval.account, observed.approval.nonce);
            let Some(payout) = gate.payouts.get_mut(&key) else { continue };
            ensure!(
                payout.approval.approved_seq == observed.approval.approved_seq,
                "custody observed another withdrawal approval"
            );
            if std::mem::take(&mut payout.recovered) {
                gate.unswept -= 1;
            }
            if observed.claimed {
                gate.schedule.retire(payout.order, payout.released);
                gate.payouts.remove(&key);
                if let Some(registry) = &gate.approvals {
                    registry.lock().map_err(|_| anyhow::anyhow!("approval registry is poisoned"))?.remove(&key);
                }
                continue;
            }
            payout.observed_epoch = Some(evidence.signer_epoch);
        }
        self.load_backlog()?;
        self.prove_loaded()?;
        let gate = self.gate.as_ref().expect("checkpoint gate exists");
        if !matches!(gate.health, Some(crate::checkpoints::Health::Healthy)) {
            return Ok(());
        }
        let Some(posted) = evidence.checkpoint_seq else { return Ok(()) };
        let Some(ts) = self.admitted_ts() else { return Ok(()) };
        let issued_at = u64::try_from(ts / 1_000).context("negative payout issue time")?;
        let gate = self.gate.as_mut().expect("checkpoint gate exists");
        if !gate.schedule.prepare(evidence.signer_epoch, issued_at, &gate.payouts) {
            return Ok(());
        }
        let mut releases = Vec::new();
        for _ in 0..crate::checkpoints::APPROVAL_PAGE_SIZE {
            let Some((order, key)) = gate.schedule.next() else { break };
            let payout = gate.payouts.get(&key).expect("an issuance identity has a pending payout");
            if payout.observed_epoch != Some(evidence.signer_epoch) {
                break;
            }
            let Some(proof) = &payout.proof else { break };
            if proof.seq > posted || payout.approval.approved_seq > posted {
                break;
            }
            ensure!(
                proof.seq != posted
                    || evidence.checkpoint_root.as_ref().is_some_and(|root| *root == proof.root.to_string()),
                "finalized approval root differs from its local proof"
            );
            ensure!(proof.approval == payout.approval && proof.verify(proof.root), "invalid durable approval proof");
            let expires_at = issued_at.checked_add(LIFETIME_SECONDS).context("payout expiry overflow")?;
            let authorization = Authorization {
                signer: evidence.signer,
                signer_epoch: evidence.signer_epoch,
                issued_at,
                expires_at,
                checkpoint_seq: proof.seq,
                checkpoint_root: proof.root,
                siblings: proof.siblings.clone(),
            };
            releases.push((key, payout.approval, authorization));
            gate.schedule.selected(order);
        }
        let batch = releases.len();
        for (key, approval, authorization) in releases {
            let mut withdrawal = self.unsigned_payout(approval, authorization);
            withdrawal.signature = self.key.sign_payout(&withdrawal)?;
            let auth = withdrawal.authorization.as_ref().expect("constructed typed payout");
            let released = (auth.signer_epoch, auth.expires_at);
            self.record(LogEntry::Withdrawal(withdrawal))?;
            let gate = self.gate.as_mut().expect("checkpoint gate exists");
            let payout = gate.payouts.get_mut(&key).expect("pending payout exists");
            gate.schedule.issued(payout.order, key, payout.released, released.1);
            payout.released = Some(released);
            info!(account = %approval.account, nonce = approval.nonce, approval_seq = approval.approved_seq, "durable withdrawal authorization issued");
        }
        if batch > 0 {
            // One sync per released batch, not per authorization: custody and the stream see a
            // signature only after `flush` syncs again, and each release used to cost two syncs on
            // the sequencer thread.
            self.log.sync()?;
        }
        Ok(())
    }

    /// Builds the common stream/log representation before the enclave signs it.
    fn unsigned_payout(&self, approval: WithdrawalApproval, authorization: Authorization) -> SignedWithdrawal {
        SignedWithdrawal {
            account: approval.account,
            amount: approval.amount,
            nonce: approval.nonce,
            chain_id: self.chain_id,
            vault: self.vault,
            signature: String::new(),
            approved_seq: Some(approval.approved_seq),
            authorization: Some(authorization),
        }
    }

    /// Builds a simulated typed payout for mock custody after the approval is synced.
    /// Error cases:
    /// - the proof is absent or logging/signing fails
    pub(super) fn sign_mock_payout(&mut self, approval: WithdrawalApproval) -> anyhow::Result<()> {
        self.checkpoint()?;
        self.sign_mock_authorization(approval)
    }

    /// Signs and logs the typed mock payout once a checkpoint covering the approval is durable.
    /// Error cases:
    /// - the proof is absent or logging/signing fails
    pub(super) fn sign_mock_authorization(&mut self, approval: WithdrawalApproval) -> anyhow::Result<()> {
        let proof =
            self.engine.approval_proof(approval.account, approval.nonce).context("mock approval proof absent")?;
        self.log.sync()?;
        let issued_at = u64::try_from(self.last_ts / 1_000)?;
        let authorization = Authorization {
            signer: AccountId(self.key.address()),
            signer_epoch: 1,
            issued_at,
            expires_at: issued_at.checked_add(LIFETIME_SECONDS).context("mock payout expiry overflow")?,
            checkpoint_seq: proof.seq,
            checkpoint_root: proof.root,
            siblings: proof.siblings,
        };
        let mut withdrawal = self.unsigned_payout(approval, authorization);
        withdrawal.signature = self.key.sign_payout(&withdrawal)?;
        self.record(LogEntry::Withdrawal(withdrawal))?;
        self.log.sync()
    }
}

/// Outstanding delivery identities, rebuilt from approvals on every recovery.
pub(super) type Payouts = BTreeMap<(AccountId, u64), Payout>;
