//! Delivery state derived from durable approvals and authenticated finalized custody evidence.
//!
//! An approval's 24-hour authorization is signed only once the approval is synced and covered by
//! the finalized checkpoint the vault accepted, while collateral reconciles and the vault halts no
//! withdrawal claim, and, while the log check applies, once a check covers it. A logged
//! signature is delivery, not payment: the vault's used nonce is. A new signer epoch (a rotation, or
//! the timelock installing this key again after a retirement) signs again, with no request, each
//! unpaid approval never signed or whose authorization is still usable; an expired one waits for its
//! account's screened request. A restart signs nothing again whose latest logged authorization is of
//! the epoch in force, and holds no release for an approval whose authorization has expired (`backlog`).
//! A holder's queued request lives in memory, so a restart forgets it; the same signed body stays good
//! until an authorization replaces the one it names.

use std::collections::BTreeMap;

use anyhow::{Context, ensure};
use engine_core::access::AccessReject;
use engine_core::eip712::B256;
use engine_core::merkle::WithdrawalApprovalProof;
use engine_core::types::AccountId;
use engine_core::withdrawal::WithdrawalApproval;
use tracing::{debug, info};

use crate::checkpoints::{APPROVAL_PAGE_SIZE, ApprovalId, LifecycleEvidence};
use crate::log::LogEntry;
use crate::sequencer::checkpoints::Gate;
use crate::sequencer::{AccountPayouts, CheckWait, PayoutView, Reissue, ReissueScreening, Sequencer, SignedWithdrawal};
use crate::withdrawals::{Authorization, LIFETIME_SECONDS};

mod backlog;
pub(super) use crate::sequencer::payouts::backlog::Backlog;

/// A signature is delivery progress; the contract's used nonce is the payment authority.
pub(super) struct Payout {
    pub approval: WithdrawalApproval,
    pub order: (u64, usize),
    pub proof: Option<WithdrawalApprovalProof>,
    pub released: Option<(u64, u64)>,
    /// The latest signed authorization, as the stream carried it; `released` is its epoch and expiry.
    pub latest: Option<SignedWithdrawal>,
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
            latest: None,
            observed_epoch: None,
            recovered: false,
            requested: false,
        }
    }

    /// Takes a newly signed authorization, or the latest one a recovered log holds, as the one in
    /// effect.
    pub fn authorize(&mut self, withdrawal: SignedWithdrawal) {
        self.released = withdrawal.authorization.as_ref().map(|auth| (auth.signer_epoch, auth.expires_at));
        self.latest = Some(withdrawal);
    }
}

/// Recovered approvals loaded, and so proved and observed, at a time.
pub(super) const BACKLOG_WINDOW: usize = 4 * APPROVAL_PAGE_SIZE;

impl Sequencer {
    /// Attempts a paced checkpoint again when `retry`: one was withheld for a cause that has cleared.
    /// Error cases:
    /// - as `paced_checkpoint`
    pub(super) fn retry_withheld(&mut self, retry: bool) -> anyhow::Result<()> {
        if retry { self.paced_checkpoint() } else { Ok(()) }
    }

    /// Loads the oldest recovered approvals once the previous window was observed, so a restart
    /// holds proofs and queries custody only for a window plus the approvals still unpaid. Nothing
    /// after the oldest unloaded live approval is signed, and an unobserved due one blocks every later
    /// release, so no newer approval passes an older one that may still need a signature.
    /// Error cases:
    /// - the backlog names an approval the engine lacks, or the registry is poisoned
    pub(super) fn load_backlog(&mut self) -> anyhow::Result<()> {
        let Some(gate) = &mut self.gate else { return Ok(()) };
        if gate.unswept > 0 {
            return Ok(());
        }
        let mut loaded = std::collections::BTreeSet::new();
        for _ in 0..BACKLOG_WINDOW {
            let Some((key, index, latest)) = gate.backlog.pop_front() else { break };
            let approval = *self
                .engine
                .withdrawal_approval(key.0, key.1)
                .context("immutable approval lacks its validated event order")?;
            let mut payout = Payout::new(approval, index);
            payout.recovered = true;
            if let Some(latest) = latest {
                payout.authorize(latest);
            }
            let released = payout.released;
            // A signed approval waits for rotation or a request; an unsigned one, or one signed by an
            // epoch other than the latest observed, is due. The epoch scan cannot see an approval
            // loaded behind its position.
            let retired = released.is_none_or(|(epoch, _)| gate.schedule.epoch().is_some_and(|latest| latest != epoch));
            gate.schedule.insert(payout.order, key, retired);
            gate.payouts.insert(key, payout);
            gate.unproved.insert(key);
            loaded.insert(key);
            gate.unswept += 1;
        }
        let unloaded = match gate.backlog.oldest_live() {
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
        // Only what was loaded here: an approval of a batch still syncing waits for its checkpoint.
        self.register_payout_keys(loaded)
    }

    /// Proves loaded approvals against the checkpoint that already covers the engine state, or
    /// writes one that does. A vault checkpoint the log holds no entry for, as a promotion's copy can
    /// start at (#595), proves nothing: no replay of the log could check an authorization's root
    /// against it, so the approvals wait for the next checkpoint written.
    /// Error cases:
    /// - the checkpoint cannot be written
    pub(super) fn prove_loaded(&mut self) -> anyhow::Result<()> {
        let Some(gate) = &self.gate else { return Ok(()) };
        if gate.unproved.is_empty() {
            return Ok(());
        }
        let held = self.dedup_checkpoint_seq == Some(gate.parent.seq);
        if gate.parent.has_checkpoint && held && Some(gate.parent.seq) == self.engine.last_seq() {
            let (seq, root) = (gate.parent.seq, gate.parent.root);
            self.capture_payout_proofs(seq, root)
        } else {
            self.paced_checkpoint()
        }
    }

    /// Captures each pending approval's first proof at the checkpoint at `seq`, of root `root`, which
    /// covers it: from the engine's tree before it is detached, and otherwise from the root worker,
    /// whose answer arrives as an outcome (`take_payout_proofs`). A payout is signed only once it has
    /// its proof.
    /// Error cases:
    /// - the root worker has stopped
    pub(super) fn capture_payout_proofs(&mut self, seq: u64, root: B256) -> anyhow::Result<()> {
        let Some(gate) = &mut self.gate else { return Ok(()) };
        let unproved = std::mem::take(&mut gate.unproved);
        if unproved.is_empty() {
            return Ok(());
        }
        if self.roots.detached() {
            return self.prove_payouts(seq, root, unproved.into_iter().collect());
        }
        for key in unproved {
            let payout = gate.payouts.get_mut(&key).expect("an unproved payout is pending");
            let approval = payout.approval;
            payout.proof = self.engine.approval_proof(approval.account, approval.nonce);
            assert!(payout.proof.is_some(), "a checkpoint includes every preceding approval");
        }
        Ok(())
    }

    /// Takes the root worker's first proofs of payouts at the checkpoint at `seq`. One the worker had
    /// no version for, as when an activation discarded the tree since, waits for the next checkpoint.
    pub(super) fn take_payout_proofs(&mut self, seq: u64, proofs: Vec<crate::root_worker::Proved>) {
        let Some(gate) = &mut self.gate else { return };
        for (key, proof) in proofs {
            // A payout claimed meanwhile needs no proof.
            let Some(payout) = gate.payouts.get_mut(&key) else { continue };
            match proof {
                Some(proof) => payout.proof = Some(proof),
                None => {
                    debug!(account = %key.0, nonce = key.1, seq, "no version proves the approval; the next checkpoint will");
                    gate.unproved.insert(key);
                }
            }
        }
    }

    /// Exposes only synced approvals to the checkpoint worker's bounded nonce-use queries.
    /// Error cases:
    /// - the worker registry is poisoned or an approval identity conflicts
    pub(super) fn register_payouts(&mut self) -> anyhow::Result<()> {
        let Some(gate) = &mut self.gate else { return Ok(()) };
        let keys = std::mem::take(&mut gate.unregistered);
        self.register_payout_keys(keys)
    }

    /// `register_payouts` for the approvals `keys` names.
    /// Error cases:
    /// - as `register_payouts`
    pub(super) fn register_payout_keys(
        &mut self,
        keys: std::collections::BTreeSet<(AccountId, u64)>,
    ) -> anyhow::Result<()> {
        let Some(gate) = &mut self.gate else { return Ok(()) };
        let Some(registry) = &gate.approvals else { return Ok(()) };
        let mut registry = registry.lock().map_err(|_| anyhow::anyhow!("approval registry is poisoned"))?;
        for key in keys {
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

    /// Releases or reissues only the identities explicitly observed unpaid at a finalized block,
    /// and none while the vault halts withdrawal claims.
    /// Error cases:
    /// - custody evidence conflicts, the approval proof is invalid, or the log cannot be written
    pub(super) fn release_payouts(&mut self, evidence: &LifecycleEvidence) -> anyhow::Result<()> {
        ensure!(
            evidence.signer_retired() || evidence.signer == AccountId(self.key.address()),
            "payout signer differs from this enclave"
        );
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
                // A recovered approval can be paid before a held checkpoint would have proved it.
                gate.unproved.remove(&key);
                if let Some(registry) = &gate.approvals {
                    registry.lock().map_err(|_| anyhow::anyhow!("approval registry is poisoned"))?.remove(&key);
                }
                continue;
            }
            payout.observed_epoch = Some(evidence.signer_epoch);
        }
        self.load_backlog()?;
        self.prove_loaded()?;
        self.settle_payout_proofs(evidence.checkpoint_seq)?;
        let gate = self.gate.as_ref().expect("checkpoint gate exists");
        if !gate.admits_withdrawals() || evidence.halts_withdrawals() {
            return Ok(());
        }
        let Some(posted) = evidence.checkpoint_seq else { return Ok(()) };
        // A restart after a crash in a segment's rotation has no segment for an authorization until the next
        // command starts one; the next evidence releases it.
        if !self.log.takes_non_command() {
            return Ok(());
        }
        let Some(ts) = self.admitted_ts(None) else { return Ok(()) };
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
            // With rotation on and two or more members in the turn's set, a signature waits for a log
            // check to verify the log through its approval (`log_check`).
            if !self.log_check.covers(&self.engine, payout.approval.approved_seq) {
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
            self.record(LogEntry::Withdrawal(withdrawal.clone()))?;
            let gate = self.gate.as_mut().expect("checkpoint gate exists");
            let payout = gate.payouts.get_mut(&key).expect("pending payout exists");
            payout.authorize(withdrawal);
            payout.requested = false;
            info!(account = %approval.account, nonce = approval.nonce, approval_seq = approval.approved_seq, "durable withdrawal authorization issued");
        }
        // With the syncs on their own thread, the batch's sync covers these: custody and the stream
        // get a signature when the batch is released, and the payouts read answers with it then too.
        // Otherwise one sync per released batch, not per authorization.
        if batch > 0 && self.pipeline.is_none() {
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
        let Some(ts) = self.admitted_ts(None) else { return Reissue::Unavailable };
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

    /// Reads an account's unpaid payouts after nonce `after`, oldest first, at most `limit`: a range
    /// scan of the gate's map that costs no log, disk or custody read.
    pub(super) fn account_payouts(&self, account: AccountId, after: Option<u64>, limit: usize) -> AccountPayouts {
        let seq = self.engine.last_seq();
        let Some(gate) = &self.gate else {
            return AccountPayouts { seq, loaded: false, payouts: Vec::new(), next: None };
        };
        let first = after.map_or(0, |after| after.saturating_add(1));
        let mut held = gate.payouts.range((account, first)..=(account, u64::MAX));
        let payouts = held
            .by_ref()
            .take(limit)
            .map(|(&(_, nonce), payout)| {
                let usable_until = self.usable_until(gate, payout.released);
                PayoutView {
                    nonce,
                    amount: payout.approval.amount,
                    withdrawal: payout.latest.clone(),
                    check: usable_until.is_none().then(|| self.log_check_wait(payout.approval.approved_seq)).flatten(),
                    usable_until,
                }
            })
            .collect::<Vec<_>>();
        let next = held.next().and(payouts.last().map(|last| last.nonce));
        AccountPayouts { seq, loaded: gate.backlog.is_empty(), payouts, next }
    }

    /// How many unpaid approvals' next signatures wait for the installation that ends the turns.
    pub(super) fn awaiting_installation(&self) -> usize {
        let Some(gate) = &self.gate else { return 0 };
        let waits = |payout: &&Payout| {
            !self.usable(gate, payout.released)
                && matches!(self.log_check_wait(payout.approval.approved_seq), Some(CheckWait::Installation { .. }))
        };
        gate.payouts.values().filter(waits).count()
    }

    /// Whether an authorization released as `released`, its signer epoch and expiry, needs no new
    /// signature, and so waits for no log check: it is of the latest signer epoch and has not expired.
    fn usable(&self, gate: &Gate, released: Option<(u64, u64)>) -> bool {
        self.usable_until(gate, released).is_some()
    }

    /// When an authorization released as `released` stops being usable (`usable`), while it is; a reissue
    /// of it is refused until then.
    fn usable_until(&self, gate: &Gate, released: Option<(u64, u64)>) -> Option<u64> {
        let now = u64::try_from(self.last_ts / 1_000).unwrap_or_default();
        released
            .filter(|&(epoch, expires_at)| {
                now < expires_at && gate.schedule.epoch().is_none_or(|latest| latest == epoch)
            })
            .map(|(_, expires_at)| expires_at)
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
        let proof = if self.roots.detached() {
            let seq = self.dedup_checkpoint_seq.context("a mock payout follows its checkpoint")?;
            let proofs = self.roots.worker.prove_now(seq, vec![(approval.account, approval.nonce)])?;
            proofs.into_iter().next().and_then(|(_, proof)| proof)
        } else {
            self.engine.approval_proof(approval.account, approval.nonce)
        };
        let proof = proof.context("mock approval proof absent")?;
        self.sync_log(true)?;
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
        self.note_signed((approval.account, approval.nonce));
        self.sync_log(true)
    }
}

/// Outstanding delivery identities, rebuilt from approvals on every recovery.
pub(super) type Payouts = BTreeMap<(AccountId, u64), Payout>;
