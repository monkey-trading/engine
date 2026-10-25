//! Approval funding provenance derived from authenticated commands, never a delivery cursor.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, ensure};
use engine_core::command::Sequenced;
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositStatus, LegacyDeposit, RecordedDeposit};
use engine_core::event::Event;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};

pub(crate) type Authorizations = BTreeMap<(AccountId, u64), u64>;

#[derive(Default)]
pub(crate) struct FinalityProof {
    pub legacy: BTreeSet<String>,
    pub receipts: BTreeMap<String, DepositReceipt>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Approval {
    seq: u64,
    amount: Fixed,
    balance_after: Fixed,
}

#[derive(Default)]
pub(crate) struct Approvals {
    credit_sequences: BTreeMap<String, u64>,
    pending: BTreeMap<(AccountId, u64), Approval>,
    #[cfg(test)]
    authorization_passes: std::sync::atomic::AtomicUsize,
}

impl FinalityProof {
    /// Retains only exact currently locked receipts, bounding storage by pending engine obligations.
    pub(crate) fn include_pending_receipt(&mut self, receipt: &DepositReceipt, recorded: Option<&RecordedDeposit>) {
        if recorded.is_some_and(|record| record.status == DepositStatus::Unfinalised && record.receipt == *receipt) {
            self.receipts.insert(receipt.reference(), receipt.clone());
        }
    }
}

impl Approvals {
    #[cfg(test)]
    pub(crate) fn authorization_passes(&self) -> usize {
        self.authorization_passes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Retains the actual prefix without summing historical money during replay.
    pub(crate) fn observe(&mut self, engine: &Engine, command: &Sequenced, events: &[Event], logged: bool) {
        if engine.deposit_finality_enabled() {
            return;
        }
        if let Some(deposit) = LegacyDeposit::from_command(&command.cmd, events) {
            self.credit_sequences.insert(deposit.chain_ref, command.seq);
        }
        if logged {
            for event in events {
                if let Event::WithdrawalApproved { account, amount, nonce } = event {
                    let balance_after = engine.accounts().get(account).expect("approval account exists").balance;
                    self.pending
                        .insert((*account, *nonce), Approval { seq: command.seq, amount: *amount, balance_after });
                }
            }
        }
    }

    /// Forgets provenance only after the exact already-validated signature tuple is recorded.
    pub(crate) fn signed(&mut self, account: AccountId, amount: Fixed, nonce: u64) {
        if self.pending.get(&(account, nonce)).is_some_and(|approval| approval.amount == amount) {
            self.pending.remove(&(account, nonce));
        }
    }

    /// Distinguishes inherited obligations from later core-approved withdrawals.
    pub(crate) fn contains(&self, account: AccountId, nonce: u64) -> bool {
        self.pending.contains_key(&(account, nonce))
    }

    /// Uses a specific proof prefix; an absent inherited key never acquires blanket authorization.
    pub(crate) fn release_seq(
        &self,
        authorized: &Authorizations,
        account: AccountId,
        nonce: u64,
        seq: u64,
    ) -> Option<u64> {
        if self.contains(account, nonce) {
            authorized.get(&(account, nonce)).map(|proved| seq.max(*proved))
        } else {
            Some(seq)
        }
    }

    /// Checks each pending reservation at its original prefix, or collectively at the current one.
    /// Error cases:
    /// - an unproved principal sum overflows or a proven lock exceeds the current lock
    pub(crate) fn authorize(
        &self,
        engine: &Engine,
        ledger: &BTreeMap<String, LegacyDeposit>,
        proof: &FinalityProof,
    ) -> anyhow::Result<Authorizations> {
        #[cfg(test)]
        let _ = self.authorization_passes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ensure!(
            ledger.keys().all(|reference| self.credit_sequences.contains_key(reference)),
            "legacy credit is missing its authenticated sequence"
        );
        let proven = |deposit: &LegacyDeposit| {
            proof.legacy.contains(&deposit.chain_ref) || engine.legacy_credit_finalised(deposit)
        };
        let mut authorized = BTreeMap::new();
        for (&(account, nonce), approval) in &self.pending {
            let unproved = ledger
                .values()
                .filter(|deposit| deposit.beneficiary == (DepositBeneficiary::Trader { account }) && !proven(deposit));
            let at_approval = sum_principal(
                unproved
                    .clone()
                    .filter(|deposit| {
                        self.credit_sequences.get(&deposit.chain_ref).is_some_and(|seq| *seq <= approval.seq)
                    })
                    .map(|deposit| deposit.amount),
            )?;
            if at_approval <= approval.balance_after {
                authorized.insert((account, nonce), approval.seq);
                continue;
            }
            let balance = engine.accounts().get(&account).map_or(Fixed::ZERO, |state| state.balance);
            let current = if engine.deposit_finality_enabled() {
                let locked = engine.accounts().get(&account).map_or(Fixed::ZERO, |state| state.unfinalised_deposits);
                let old_proved = ledger
                    .values()
                    .filter(|deposit| {
                        deposit.beneficiary == (DepositBeneficiary::Trader { account })
                            && proof.legacy.contains(&deposit.chain_ref)
                            && engine.unresolved_legacy_credit(&deposit.chain_ref) == Some(*deposit)
                    })
                    .map(|deposit| deposit.amount);
                let receipts = engine.unfinalised_deposits(0, u64::MAX);
                let new_proved = receipts
                    .iter()
                    .filter(|receipt| {
                        receipt.beneficiary == (DepositBeneficiary::Trader { account })
                            && proof.receipts.get(&receipt.reference()) == Some(*receipt)
                    })
                    .map(|receipt| receipt.amount);
                let proved = sum_principal(old_proved.chain(new_proved))?;
                ensure!(proved <= locked, "legacy finality proof exceeds committed principal lock");
                Fixed::from_raw(locked.raw().checked_sub(proved.raw()).context("legacy lock subtraction overflow")?)
            } else {
                sum_principal(unproved.map(|deposit| deposit.amount))?
            };
            if current <= balance {
                authorized.insert((account, nonce), engine.last_seq().context("approval without sequence")?);
            }
        }
        Ok(authorized)
    }

    /// Requires an authorization for every historical obligation, including checkpoint-deferred ones.
    pub(crate) fn all_authorized(&self, authorized: &Authorizations) -> bool {
        self.pending.keys().all(|key| authorized.contains_key(key))
    }
}

/// Sums only eligibility inputs; replaying old execution does not acquire a new overflow failure.
/// Error cases:
/// - the total cannot be represented as Fixed
fn sum_principal(mut amounts: impl Iterator<Item = Fixed>) -> anyhow::Result<Fixed> {
    amounts
        .try_fold(0i128, |sum, amount| sum.checked_add(amount.raw()).context("legacy principal overflow"))
        .map(Fixed::from_raw)
}

#[cfg(test)]
mod tests;
