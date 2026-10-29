//! Delivery state derived from durable approvals and authenticated finalized custody evidence.

use std::collections::BTreeMap;

use anyhow::{Context, ensure};
use engine_core::access::AccessReject;
use engine_core::merkle::WithdrawalApprovalProof;
use engine_core::types::AccountId;
use engine_core::withdrawal::WithdrawalApproval;
use tracing::{debug, info};

use crate::checkpoints::{APPROVAL_PAGE_SIZE, ApprovalId, LifecycleEvidence};
use crate::log::LogEntry;
use crate::sequencer::{Reissue, ReissueScreening, Sequencer, SignedWithdrawal};
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
    /// Its account's screened request for a fresh authorization waits to be signed.
    pub requested: bool,
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
            requested: false,
        }
    }
}

/// Recovered approvals loaded, and so proved and observed, at a time.
pub(super) const BACKLOG_WINDOW: usize = 4 * APPROVAL_PAGE_SIZE;

impl Sequencer {
    /// Loads the oldest recovered approvals once the previous window was observed, so a restart
    /// holds proofs and queries custody only for a window plus the approvals still unpaid. Nothing
    /// after the oldest unloaded approval is signed, and an unobserved due one blocks every later
    /// release, so no newer approval passes an older one that may still need a signature.
    /// Error cases:
    /// - the backlog names an approval the engine lacks, or the registry is poisoned
    pub(super) fn load_backlog(&mut self) -> anyhow::Result<()> {
        let Some(gate) = &mut self.gate else { return Ok(()) };
        if gate.unswept > 0 {
            return Ok(());
        }
        for _ in 0..BACKLOG_WINDOW {
            let Some((key, index, released)) = gate.backlog.pop_front() else { break };
            let approval = *self
                .engine
                .withdrawal_approval(key.0, key.1)
                .context("immutable approval lacks its validated event order")?;
            let mut payout = Payout::new(approval, index);
            payout.recovered = true;
            payout.released = released;
            // A signed approval waits for rotation or a request; an unsigned one, or one signed by an
            // epoch other than the latest observed, is due. The epoch scan cannot see an approval
            // loaded behind its position.
            let retired = released.is_none_or(|(epoch, _)| gate.schedule.epoch().is_some_and(|latest| latest != epoch));
            gate.schedule.insert(payout.order, key, retired);
            gate.payouts.insert(key, payout);
            gate.unproved.insert(key);
            gate.unregistered.insert(key);
            gate.unswept += 1;
        }
        let unloaded = match gate.backlog.front() {
            Some(&(key, index, _)) => Some((
                self.engine
                    .withdrawal_approval(key.0, key.1)
                    .context("immutable approval lacks its validated event order")?
                    .approved_seq,
                index,
            )),
            None => None,
        };
        gate.schedule.hold_after(unloaded);
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
                gate.schedule.retire(payout.order);
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
        gate.schedule.prepare(evidence.signer_epoch, &gate.payouts);
        let mut releases = Vec::new();
        for _ in 0..crate::checkpoints::APPROVAL_PAGE_SIZE {
            let Some((order, key)) = gate.schedule.next() else { break };
            let payout = gate.payouts.get(&key).expect("an issuance identity has a pending payout");
            // A rotation, or a restart that finds a retired epoch, has no request to screen, so it
            // replaces only an authorization its account can still use; an expired one waits for
            // the account's screened request.
            if !payout.requested && payout.released.is_some_and(|(_, expires_at)| expires_at <= issued_at) {
                debug!(account = %key.0, nonce = key.1, "expired authorization left for its account's request");
                gate.schedule.dismiss(order);
                continue;
            }
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
            payout.released = Some(released);
            payout.requested = false;
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

    /// Queues an authenticated holder's request to sign an approval again once its last
    /// authorization expired unclaimed or was signed by a retired epoch; the signature follows with
    /// the next custody evidence, in FIFO order. Once access is enforced, `screening` must pass the
    /// checks a new withdrawal gets, at this time; a refused account gets no fresh authorization and
    /// its approval stays unpaid. `replaces` must be the expiry of the latest authorization, so one
    /// signed request is good for one reissue. Expiry alone never reissues, so unclaimed approvals
    /// cost nothing until their holder asks.
    pub(super) fn request_reissue(
        &mut self,
        account: AccountId,
        nonce: u64,
        replaces: u64,
        screening: Option<&ReissueScreening>,
    ) -> Reissue {
        let Some(ts) = self.admitted_ts() else { return Reissue::Unavailable };
        if self.engine.protocol_version().supports_access() {
            let refusal = match screening {
                Some(screening) => {
                    self.engine.client_screening_refusal(account, screening.request_hash, &screening.evidence, ts)
                }
                None => Some(AccessReject::EvidenceMissing),
            };
            if let Some(reason) = refusal {
                return Reissue::Refused { reason };
            }
        }
        let Some(gate) = &mut self.gate else { return Reissue::Unavailable };
        let Some(payout) = gate.payouts.get_mut(&(account, nonce)) else { return Reissue::Unknown };
        let order = payout.order;
        let current = payout.released.map_or(0, |(_, expires_at)| expires_at);
        if replaces != current {
            return Reissue::Stale { current };
        }
        let now = u64::try_from(ts / 1_000).unwrap_or_default();
        if let Some((epoch, expires_at)) = payout.released
            && now < expires_at
            && gate.schedule.epoch().is_none_or(|latest| latest == epoch)
        {
            return Reissue::StillValid { expires_at };
        }
        payout.requested = true;
        if !gate.schedule.is_pending(order) {
            gate.schedule.request(order);
            info!(%account, nonce, "withdrawal reissue requested");
        }
        Reissue::Queued
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
