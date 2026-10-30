//! Atomic canonical receipt replacement, including V26 payer screening.

use alloy_primitives::B256;

use crate::Fixed;
use crate::access::{CreditScreening, UncreditedReceipt};
use crate::deposits::{DepositBeneficiary, DepositReceipt, DepositReject, DepositStatus};
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;

impl Engine {
    /// Moves a live receipt's transaction-local deposit to its canonical replacement in one step:
    /// the orphan is reversed and the replacement credited, or reinstated, so cash and the principal
    /// lock net out and no close-out runs. Redelivery after the move is a no-op.
    /// Error cases:
    /// - the orphan is unknown, or not live at the observed revision
    /// - the replacement is invalid, at or below the finalised frontier, or not the same
    ///   transaction-local deposit with the same beneficiary and amount
    /// - the replacement is recorded but not reversed at the named revision
    pub(super) fn replace_deposit(
        &mut self,
        orphan: &DepositReceipt,
        observed_at_seq: u64,
        replacement: &DepositReceipt,
        replacement_reversed_at_seq: Option<u64>,
        screening: CreditScreening<'_>,
        events: &mut Vec<Event>,
    ) -> Result<(), DepositReject> {
        let recorded = self.deposits.get(&orphan.reference()).ok_or(DepositReject::UnknownReceipt)?;
        let uncredited = recorded.uncredited;
        let current = self.deposits.get(&replacement.reference());
        if recorded.status == DepositStatus::Reversed
            && current.is_some_and(|d| d.receipt == *replacement && d.status != DepositStatus::Reversed)
        {
            return Ok(());
        }
        if recorded.status != DepositStatus::Unfinalised || recorded.updated_at_seq != observed_at_seq {
            return Err(DepositReject::SettledReceipt);
        }
        if replacement.block_hash == B256::ZERO
            || replacement.tx_hash == B256::ZERO
            || replacement.block == u64::MAX
            || replacement.reference() == orphan.reference()
        {
            return Err(DepositReject::InvalidReceipt);
        }
        if replacement.transaction_reference() != orphan.transaction_reference()
            || replacement.beneficiary != orphan.beneficiary
            || replacement.amount != orphan.amount
        {
            return Err(DepositReject::ConflictingReceipt);
        }
        if self.custody_checkpoint.is_some_and(|checkpoint| replacement.block <= checkpoint.block) {
            return Err(DepositReject::SettledReceipt);
        }
        match (current, replacement_reversed_at_seq) {
            (None, None) => {}
            (Some(d), _) if d.receipt != *replacement => return Err(DepositReject::ConflictingReceipt),
            (Some(d), Some(seq)) if d.status == DepositStatus::Reversed && d.updated_at_seq == seq => {}
            (None, Some(_)) => return Err(DepositReject::UnknownReceipt),
            (Some(_), _) => return Err(DepositReject::SettledReceipt),
        }
        self.reverse_conflicting_receipts(replacement, events)?;
        if self.protocol_version >= ProtocolVersion::V26 {
            return self.replace_screened_deposit(orphan, replacement, uncredited, screening, events);
        }
        if let Some(uncredited) = uncredited {
            // The replacement is the same funded deposit, so it keeps the orphan's refusal.
            self.record_deposit(orphan, DepositStatus::Reversed, Fixed::ZERO, Some(uncredited), events)?;
            self.track_uncredited(orphan, uncredited, false);
            self.record_deposit(replacement, DepositStatus::Unfinalised, Fixed::ZERO, Some(uncredited), events)?;
            self.track_uncredited(replacement, uncredited, true);
            return Ok(());
        }
        self.record_deposit(orphan, DepositStatus::Reversed, -orphan.amount, None, events)?;
        self.record_deposit(replacement, DepositStatus::Unfinalised, replacement.amount, None, events)?;
        Ok(())
    }

    /// Replaces a receipt without transient cash removal; an authenticated unchanged payer retains
    /// its credit on curable source failures, while terminal refusals still dominate.
    /// Error cases:
    /// - the change in credited principal cannot be represented
    fn replace_screened_deposit(
        &mut self,
        orphan: &DepositReceipt,
        replacement: &DepositReceipt,
        old: Option<UncreditedReceipt>,
        screening: CreditScreening<'_>,
        events: &mut Vec<Event>,
    ) -> Result<(), DepositReject> {
        let known_payer = self.deposits.get(&orphan.reference()).and_then(|recorded| recorded.payer);
        let retained_credit = old.is_none() && known_payer.is_some() && known_payer == screening.payer;
        let new = self.terminal_receipt_hold(replacement).or_else(|| {
            self.receipt_refusal(replacement, screening)
                .filter(|refused| !retained_credit || refused.reason.holds_deposit())
        });
        if old.is_none() != new.is_none() {
            self.move_deposit_cash(replacement, new.is_none(), true)?;
        }
        self.record_deposit(
            orphan,
            DepositStatus::Reversed,
            if old.is_none() { -orphan.amount } else { Fixed::ZERO },
            old,
            events,
        )?;
        if let Some(old) = old {
            self.track_uncredited(orphan, old, false);
        }
        self.record_deposit(
            replacement,
            DepositStatus::Unfinalised,
            if new.is_none() { replacement.amount } else { Fixed::ZERO },
            new,
            events,
        )?;
        self.deposits.get_mut(&replacement.reference()).expect("replacement recorded").payer = screening.payer;
        if let Some(new) = new {
            self.track_uncredited(replacement, new, true);
            self.announce_uncredited(replacement, new, events);
        }
        if old.is_none()
            && new.is_some()
            && let DepositBeneficiary::Trader { account } = replacement.beneficiary
        {
            self.close_out_after_balance_change(account, events);
        }
        Ok(())
    }
}
