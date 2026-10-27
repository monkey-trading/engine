//! Access screening of quorum-proven custody receipts from `V24`. A refused receipt stays in the
//! receipt lifecycle, finalised, reversed, reinstated and replaced like any other, but its principal
//! never enters cash or the withdrawal lock: it is held for good, or pending until a retry credits
//! it, locked until finality while the receipt is still unfinalised.

use crate::Fixed;
use crate::access::engine::pending_event;
use crate::access::{AccessDeposit, AccessEvidence, CreditScreening, HeldDeposit, UncreditedReceipt};
use crate::deposits::{DepositBeneficiary, DepositReceipt, DepositReject, DepositStatus, DepositUpdate};
use crate::eip712::deposit_screening_hash;
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;
use crate::types::AccountId;

/// What access screening sees of a receipt: its beneficiary, funding address, amount and exact
/// reference, which the source's evidence binds.
pub fn receipt_access_deposit(receipt: &DepositReceipt, payer: Option<AccountId>) -> AccessDeposit {
    AccessDeposit {
        account: match receipt.beneficiary {
            DepositBeneficiary::Trader { account } => Some(account),
            DepositBeneficiary::House => None,
        },
        payer,
        amount: receipt.amount,
        chain_ref: receipt.reference(),
    }
}

impl Engine {
    /// Credits a new receipt only on evidence that passes the access rules for its beneficiary and
    /// funding address; otherwise records it uncredited, pending or held.
    pub(crate) fn access_custody_deposit(
        &mut self,
        receipt: &DepositReceipt,
        payer: Option<AccountId>,
        evidence: &AccessEvidence,
        events: &mut Vec<Event>,
    ) {
        if !self.protocol_version.supports_access() {
            events.push(Event::CustodyDepositRejected {
                receipt: receipt.clone(),
                reason: DepositReject::ProtocolNotActive,
            });
            return;
        }
        let fresh = !self.deposits.contains_key(&receipt.reference());
        let update = DepositUpdate::Credit { receipt: receipt.clone() };
        self.update_deposit(&update, CreditScreening { payer, evidence: Some(evidence) }, events);
        if fresh
            && self.protocol_version >= ProtocolVersion::V26
            && let Some(recorded) = self.deposits.get_mut(&receipt.reference())
        {
            recorded.payer = payer;
        }
    }

    /// Screens an atomic replacement from V26, preserving the legacy transition's replay.
    pub(crate) fn access_custody_replacement(
        &mut self,
        update: &DepositUpdate,
        payer: Option<AccountId>,
        evidence: &AccessEvidence,
        events: &mut Vec<Event>,
    ) {
        if self.protocol_version < ProtocolVersion::V26 || !matches!(update, DepositUpdate::Replace { .. }) {
            events.push(Event::CustodyDepositRejected {
                receipt: update.subject().clone(),
                reason: DepositReject::ProtocolNotActive,
            });
            return;
        }
        self.update_deposit(update, CreditScreening { payer, evidence: Some(evidence) }, events);
    }

    /// A terminal hold follows its transaction-local deposit across canonical receipt variants.
    pub(crate) fn terminal_receipt_hold(&self, receipt: &DepositReceipt) -> Option<UncreditedReceipt> {
        if self.protocol_version < ProtocolVersion::V26 {
            return None;
        }
        if let Some(indexes) = &self.hot_indexes {
            return indexes.terminal_deposit_holds.get(&receipt.transaction_reference()).copied();
        }
        self.deposits.values().find_map(|recorded| {
            (recorded.receipt.transaction_reference() == receipt.transaction_reference())
                .then_some(recorded.uncredited)
                .flatten()
                .filter(|held| held.reason.holds_deposit())
        })
    }

    /// The refusal that keeps a new receipt uncredited, from `V24`; a receipt sequenced without
    /// screening has no payer or evidence to pass.
    pub(crate) fn receipt_refusal(
        &self,
        receipt: &DepositReceipt,
        screening: CreditScreening<'_>,
    ) -> Option<UncreditedReceipt> {
        if !self.protocol_version.supports_access() {
            return None;
        }
        if let Some(held) = self.terminal_receipt_hold(receipt) {
            return Some(held);
        }
        let deposit = receipt_access_deposit(receipt, screening.payer);
        let hash = deposit_screening_hash(&deposit);
        let refused = self.check_access(deposit.account, deposit.payer, hash, screening.evidence, true, true).err();
        refused.map(|reason| UncreditedReceipt { payer: screening.payer, reason })
    }

    /// Adds a live uncredited receipt to the pending or held collateral, or removes it once it is
    /// reversed or replaced, so those sets hold exactly the uncredited funds the vault has.
    pub(crate) fn track_uncredited(&mut self, receipt: &DepositReceipt, uncredited: UncreditedReceipt, live: bool) {
        let deposit = receipt_access_deposit(receipt, uncredited.payer);
        if uncredited.reason.holds_deposit() {
            if live {
                self.held_deposits
                    .insert(deposit.chain_ref.clone(), HeldDeposit { deposit, reason: uncredited.reason });
            } else {
                self.held_deposits.remove(&deposit.chain_ref);
            }
        } else if live {
            self.pending_deposits.insert(deposit.chain_ref.clone(), deposit.clone());
            self.index_pending(&deposit, true);
        } else if let Some(pending) = self.pending_deposits.remove(&deposit.chain_ref) {
            self.index_pending(&pending, false);
        }
    }

    /// Publishes a receipt's refusal with the public pending or held event.
    pub(crate) fn announce_uncredited(
        &self,
        receipt: &DepositReceipt,
        uncredited: UncreditedReceipt,
        events: &mut Vec<Event>,
    ) {
        let deposit = receipt_access_deposit(receipt, uncredited.payer);
        if uncredited.reason.holds_deposit() {
            events.push(Event::DepositHeld {
                chain_ref: deposit.chain_ref,
                account: deposit.account,
                payer: deposit.payer,
                amount: deposit.amount,
                reason: uncredited.reason,
            });
        } else {
            events.push(pending_event(&deposit, uncredited.reason));
        }
    }

    /// Screens a pending receipt again: clear evidence credits it, locked until finality while the
    /// receipt is unfinalised; a confirmed sanction holds it; any other refusal leaves it pending.
    pub(crate) fn rescreen_receipt(
        &mut self,
        deposit: &AccessDeposit,
        evidence: &AccessEvidence,
        events: &mut Vec<Event>,
    ) {
        let recorded = self.deposits.get(&deposit.chain_ref).cloned().expect("a pending receipt is recorded");
        let pending = recorded.uncredited.expect("a pending receipt is uncredited");
        let hash = deposit_screening_hash(deposit);
        match self.check_access(deposit.account, deposit.payer, hash, Some(evidence), true, true) {
            Ok(()) => {
                let lock = recorded.status == DepositStatus::Unfinalised;
                if let Err(reason) = self.move_deposit_cash(&recorded.receipt, true, lock) {
                    events.push(Event::CustodyDepositRejected { receipt: recorded.receipt, reason });
                    return;
                }
                // Finalisation settled while the receipt waited uncredited and released no payout
                // capacity; crediting it now restores the principal a finalised receipt contributes.
                if !lock && let Some(cash) = &mut self.withdrawal_cash {
                    *cash += recorded.receipt.amount;
                }
                self.track_uncredited(&recorded.receipt, pending, false);
                self.record_deposit(&recorded.receipt, recorded.status, recorded.receipt.amount, None, events);
            }
            Err(reason) if reason.holds_deposit() => {
                let held = UncreditedReceipt { payer: deposit.payer, reason };
                self.track_uncredited(&recorded.receipt, pending, false);
                self.record_deposit(&recorded.receipt, recorded.status, Fixed::ZERO, Some(held), events);
                self.track_uncredited(&recorded.receipt, held, true);
                self.announce_uncredited(&recorded.receipt, held, events);
            }
            Err(reason) => events.push(pending_event(deposit, reason)),
        }
    }
}
