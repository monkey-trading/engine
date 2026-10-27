//! Deterministic access decisions and custody holds.

use std::collections::BTreeMap;
use std::ops::Bound;

use crate::access::{
    AccessDeposit, AccessEvidence, AccessPolicy, AccessReject, EligibilityClass, EligibilityRequest, HeldDeposit,
    Jurisdiction, MAX_EVIDENCE_LEAD_MS, SourceFailure,
};
use crate::auth::ClientAuth;
use crate::command::Command;
use crate::deposits::legacy_chain_reference;
use crate::eip712::{self, B256, Eip712Domain, deposit_screening_hash, eligibility_hash};
use crate::engine::Engine;
use crate::event::{Event, Reject};
use crate::protocol::ProtocolVersion;
use crate::types::AccountId;

impl Engine {
    /// Dispatches the access-screened commands and, from `V24`, wraps raw funding ingress without
    /// a payer as payer-less access deposits. Returns false for every other command.
    pub(crate) fn access_dispatch(&mut self, cmd: &Command, events: &mut Vec<Event>) -> bool {
        match cmd {
            Command::AttestEligibility { request, evidence } => self.attest_eligibility(request, evidence, events),
            Command::RetryDepositScreening { chain_ref, evidence } => {
                self.retry_deposit_screening(chain_ref, evidence, events)
            }
            Command::AccessOrder { request, evidence } => self.place_order(request, Some(evidence), events),
            Command::AccessWithdraw { request, evidence, payout_available } => self.withdraw(
                request.account,
                request.amount,
                request.auth.as_ref(),
                Some((evidence, *payout_available)),
                events,
            ),
            Command::AccessDeposit { deposit, evidence } => self.access_deposit(deposit, Some(evidence), events),
            Command::AccessCustodyDeposit { receipt, payer, evidence } => {
                self.access_custody_deposit(receipt, *payer, evidence, events)
            }
            Command::AccessCustodyReplacement { update, payer, evidence } => {
                self.access_custody_replacement(update, *payer, evidence, events)
            }
            Command::Deposit { account, amount, chain_ref } if self.protocol_version.supports_access() => {
                let deposit = AccessDeposit {
                    account: Some(*account),
                    payer: None,
                    amount: *amount,
                    chain_ref: chain_ref.clone(),
                };
                self.access_deposit(&deposit, None, events);
            }
            Command::HouseCapital { amount, chain_ref }
                if self.protocol_version.supports_access()
                    && (chain_ref != "genesis"
                        || self.held_deposits.contains_key(chain_ref)
                        || self.pending_deposits.contains_key(chain_ref)) =>
            {
                let deposit =
                    AccessDeposit { account: None, payer: None, amount: *amount, chain_ref: chain_ref.clone() };
                self.access_deposit(&deposit, None, events);
            }
            _ => return false,
        }
        true
    }

    /// The access source and freshness bound the public log records.
    pub fn access_policy(&self) -> Option<&AccessPolicy> {
        self.access_policy.as_ref()
    }

    /// The account's signed eligibility declaration, if recorded.
    pub fn eligibility(&self, account: AccountId) -> Option<EligibilityClass> {
        self.eligibility.get(&account).copied()
    }

    /// Uncredited collateral held in custody, keyed by its unique chain reference.
    pub fn held_deposits(&self) -> &BTreeMap<String, HeldDeposit> {
        &self.held_deposits
    }

    /// Uncredited deposits awaiting a declaration, a classification, or a working source.
    pub fn pending_deposits(&self) -> &BTreeMap<String, AccessDeposit> {
        &self.pending_deposits
    }

    /// Whether a deposit for `account` is pending, in logarithmic time once the derived indexes exist.
    pub fn has_pending_deposit(&self, account: AccountId) -> bool {
        match &self.hot_indexes {
            Some(indexes) => indexes.pending.contains_key(&Some(account)),
            None => self.pending_deposits.values().any(|deposit| deposit.account == Some(account)),
        }
    }

    /// Up to `limit` pending deposits a retry could settle (the house's and declared accounts'), in
    /// reference order after `after`, wrapping around. Bounded by `limit` once the derived indexes
    /// exist; before then, as on an engine just decoded and not yet applied to, it scans.
    pub fn retryable_pending(&self, after: &str, limit: usize) -> Vec<(String, AccessDeposit)> {
        match &self.hot_indexes {
            Some(indexes) => self.retryable_among(
                indexes
                    .retryable
                    .range::<str, _>((Bound::Excluded(after), Bound::Unbounded))
                    .chain(indexes.retryable.range::<str, _>((Bound::Unbounded, Bound::Included(after)))),
                limit,
            ),
            None => self.retryable_among(
                self.pending_deposits
                    .range::<str, _>((Bound::Excluded(after), Bound::Unbounded))
                    .chain(self.pending_deposits.range::<str, _>((Bound::Unbounded, Bound::Included(after))))
                    .map(|(reference, _)| reference),
                limit,
            ),
        }
    }

    /// The first `limit` of `references` whose pending deposit a retry could settle.
    fn retryable_among<'a>(
        &'a self,
        references: impl Iterator<Item = &'a String>,
        limit: usize,
    ) -> Vec<(String, AccessDeposit)> {
        references
            .filter_map(|reference| {
                let deposit = self.pending_deposits.get(reference).expect("indexed references are pending");
                deposit
                    .account
                    .is_none_or(|account| self.eligibility.contains_key(&account))
                    .then(|| (reference.clone(), deposit.clone()))
            })
            .take(limit)
            .collect()
    }

    /// The recorded deployment domain, including in unsigned development logs.
    pub fn access_domain(&self) -> Option<Eip712Domain> {
        self.client_auth
            .as_ref()
            .and_then(ClientAuth::domain)
            .or_else(|| self.operator.map(|operator| eip712::domain(operator.chain_id, operator.vault)))
    }

    /// Verifies a mandatory declaration signature and consumes the signer's nonce before refusal.
    pub(crate) fn attest_eligibility(
        &mut self,
        request: &EligibilityRequest,
        evidence: &AccessEvidence,
        events: &mut Vec<Event>,
    ) {
        let result = self.accept_eligibility(request, evidence);
        match result {
            Ok(()) => events.push(Event::EligibilityAttested { account: request.account, class: request.class }),
            Err(reason) => events.push(Event::EligibilityRejected { account: request.account, reason }),
        }
    }

    /// Records one authenticated declaration screened like an order; unsigned mode grants no
    /// exemption.
    /// Error cases:
    /// - the protocol is inactive, or the signature/nonce/expiry fails
    /// - the screening evidence is missing, mismatched, stale, unknown, or excludes the account or its
    ///   jurisdiction
    /// - a declaration exists
    fn accept_eligibility(&mut self, request: &EligibilityRequest, evidence: &AccessEvidence) -> Result<(), Reject> {
        if !self.protocol_version.supports_access() {
            return Err(access_error(AccessReject::ProtocolInactive));
        }
        let domain = self.access_domain().ok_or(Reject::InvalidSignature)?;
        let hash =
            eligibility_hash(&domain, request.account, request.class, request.auth.nonce, request.auth.expires_at);
        if eip712::recover(&hash, &request.auth.signature) != Some(request.account) {
            return Err(Reject::InvalidSignature);
        }
        self.use_nonce(request.account, &request.auth)?;
        self.check_access(Some(request.account), None, hash, Some(evidence), false, false).map_err(access_error)?;
        if self.eligibility.contains_key(&request.account) {
            return Err(access_error(AccessReject::AlreadyAttested));
        }
        self.eligibility.insert(request.account, request.class);
        self.index_declaration(request.account);
        Ok(())
    }

    /// Checks all logged screening inputs, then, where `needs_declaration`, the declaration required
    /// before an order or deposit.
    /// Error cases:
    /// - policy/evidence is missing, mismatched, stale, unknown, or excludes the action
    pub(crate) fn check_access(
        &self,
        account: Option<AccountId>,
        payer: Option<AccountId>,
        request_hash: B256,
        evidence: Option<&AccessEvidence>,
        deposit: bool,
        needs_declaration: bool,
    ) -> Result<(), AccessReject> {
        if !self.protocol_version.supports_access() {
            return if evidence.is_some() { Err(AccessReject::ProtocolInactive) } else { Ok(()) };
        }
        self.screen_access(account, payer, request_hash, evidence, deposit).and_then(|()| {
            if needs_declaration && account.is_some_and(|account| !self.eligibility.contains_key(&account)) {
                Err(AccessReject::EligibilityRequired)
            } else {
                Ok(())
            }
        })
    }

    /// Applies the source's recorded classifications without consulting external policy on replay.
    /// Error cases:
    /// - any missing, stale, mismatched or excluding classification
    fn screen_access(
        &self,
        account: Option<AccountId>,
        payer: Option<AccountId>,
        request_hash: B256,
        evidence: Option<&AccessEvidence>,
        deposit: bool,
    ) -> Result<(), AccessReject> {
        if deposit && payer.is_none() {
            return Err(AccessReject::PayerMissing);
        }
        let evidence = evidence.ok_or(AccessReject::EvidenceMissing)?;
        let screening = match evidence {
            AccessEvidence::Screened(screening) => screening,
            AccessEvidence::Failed { reason } => {
                return Err(match reason {
                    SourceFailure::NotConfigured => AccessReject::SourceNotConfigured,
                    SourceFailure::MissingPeer => AccessReject::PeerUnknown,
                    SourceFailure::Unavailable => AccessReject::SourceUnavailable,
                    SourceFailure::InvalidResponse => AccessReject::SourceInvalid,
                });
            }
        };
        let policy = self.access_policy.as_ref().ok_or(AccessReject::PolicyMissing)?;
        if screening.source != policy.source
            || screening.request_hash != request_hash
            || screening.account != account
            || screening.payer != payer
        {
            return Err(AccessReject::EvidenceMismatch);
        }
        let domain = self.access_domain().ok_or(AccessReject::EvidenceMismatch)?;
        if eip712::recover(&eip712::screening_hash(&domain, screening), &screening.signature) != Some(policy.source) {
            return Err(AccessReject::SourceInvalid);
        }
        if screening.observed_at > self.now + MAX_EVIDENCE_LEAD_MS
            || self.now.saturating_sub(screening.observed_at) > policy.max_age_ms
        {
            return Err(AccessReject::EvidenceStale);
        }
        if let Some(reason) = screening.sanctions_refusal(account.is_some(), deposit) {
            return Err(reason);
        }
        if !deposit {
            match screening.jurisdiction {
                Jurisdiction::Allowed => {}
                Jurisdiction::UnitedStates => return Err(AccessReject::UnitedStates),
                Jurisdiction::Sanctioned => return Err(AccessReject::SanctionedJurisdiction),
                Jurisdiction::Unknown => return Err(AccessReject::JurisdictionUnknown),
            }
        }
        Ok(())
    }

    /// Credits once, records a curable refusal as pending, or stores an uncredited hold.
    pub(crate) fn access_deposit(
        &mut self,
        deposit: &AccessDeposit,
        evidence: Option<&AccessEvidence>,
        events: &mut Vec<Event>,
    ) {
        if !deposit.amount.is_valid_input() {
            match deposit.account {
                Some(account) => events.push(Event::DepositRejected {
                    account,
                    amount: deposit.amount,
                    chain_ref: deposit.chain_ref.clone(),
                    reason: Reject::InvalidAmount,
                }),
                None => events.push(Event::AdminRejected { reason: "access deposit amount out of range".into() }),
            }
            return;
        }
        // A duplicate, or an EVM reference that receipt finality accepts only as an exact receipt, is
        // refused as the unscreened routes refuse it.
        if self.is_credited(&deposit.chain_ref)
            || (self.deposit_finality_enabled() && legacy_chain_reference(&deposit.chain_ref).is_some())
        {
            match deposit.account {
                Some(account) => self.deposit(account, deposit.amount, &deposit.chain_ref, events),
                None => self.house_capital(deposit.amount, &deposit.chain_ref, events),
            }
            return;
        }
        if let Some(pending) = self.pending_deposits.get(&deposit.chain_ref) {
            events.push(pending_event(pending, AccessReject::DepositPending));
            return;
        }
        self.screen_deposit(deposit, evidence, events);
    }

    /// Rechecks only an immutable pending input; held and credited references cannot be released.
    pub(crate) fn retry_deposit_screening(
        &mut self,
        reference: &str,
        evidence: &AccessEvidence,
        events: &mut Vec<Event>,
    ) {
        let Some(deposit) =
            self.pending_deposits.get(reference).cloned().filter(|_| self.protocol_version.supports_access())
        else {
            events.push(Event::AdminRejected { reason: "deposit is not awaiting screening".into() });
            return;
        };
        if self.deposits.contains_key(reference) {
            return self.rescreen_receipt(&deposit, evidence, events);
        }
        self.screen_deposit(&deposit, Some(evidence), events);
    }

    /// Moves a screened input between pending, held and credited custody states. Only a confirmed
    /// sanction or a missing payer holds it; every curable refusal leaves it pending for a retry.
    fn screen_deposit(&mut self, deposit: &AccessDeposit, evidence: Option<&AccessEvidence>, events: &mut Vec<Event>) {
        let reason = if !self.protocol_version.supports_access() {
            Some(AccessReject::ProtocolInactive)
        } else if self.held_deposits.contains_key(&deposit.chain_ref) {
            Some(AccessReject::DepositHeld)
        } else {
            self.check_access(deposit.account, deposit.payer, deposit_screening_hash(deposit), evidence, true, true)
                .err()
        };
        let terminal = |reason: &AccessReject| {
            reason.holds_deposit() || matches!(reason, AccessReject::DepositHeld | AccessReject::ProtocolInactive)
        };
        if let Some(reason) = reason.filter(|reason| !terminal(reason)) {
            self.pending_deposits.insert(deposit.chain_ref.clone(), deposit.clone());
            self.index_pending(deposit, true);
            events.push(pending_event(deposit, reason));
            return;
        }
        if let Some(pending) = self.pending_deposits.remove(&deposit.chain_ref) {
            self.index_pending(&pending, false);
        }
        if let Some(reason) = reason {
            if self.protocol_version.supports_access() && !self.is_credited(&deposit.chain_ref) {
                self.held_deposits
                    .entry(deposit.chain_ref.clone())
                    .or_insert_with(|| HeldDeposit { deposit: deposit.clone(), reason });
            }
            events.push(Event::DepositHeld {
                chain_ref: deposit.chain_ref.clone(),
                account: deposit.account,
                payer: deposit.payer,
                amount: deposit.amount,
                reason,
            });
            return;
        }
        match deposit.account {
            Some(account) => self.deposit(account, deposit.amount, &deposit.chain_ref, events),
            None => self.house_capital(deposit.amount, &deposit.chain_ref, events),
        }
    }
}

/// Embeds a typed access refusal in the existing client rejection events.
pub(crate) fn access_error(reason: AccessReject) -> Reject {
    Reject::Access { reason }
}

/// Public evidence preserves the original funding input on every pending refusal.
pub(super) fn pending_event(deposit: &AccessDeposit, reason: AccessReject) -> Event {
    Event::DepositPending {
        chain_ref: deposit.chain_ref.clone(),
        account: deposit.account,
        payer: deposit.payer,
        amount: deposit.amount,
        reason,
    }
}
