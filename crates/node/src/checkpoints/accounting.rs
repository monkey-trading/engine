//! Fixed-size custody accounting derived from authenticated engine output, never host metadata.

use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::access::{AccessDeposit, AccessReject};
use engine_core::command::Command;
use engine_core::deposits::{DepositReceipt, DepositStatus, DepositUpdate};
use engine_core::event::Event;
use serde::{Deserialize, Serialize};
use tracing::error;

/// Cash, unpaid approvals, allocated distributions, pending and held collateral equal cumulative custody credits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Accounting {
    pub(super) liabilities: Fixed,
    pub(super) deposits: u64,
    /// Absent from snapshots taken before access screening, when no collateral could be held.
    #[serde(default)]
    pub(super) held: Fixed,
    /// Absent from snapshots taken before access screening, when no deposit could be pending.
    #[serde(default)]
    pub(super) pending: Fixed,
    unpaid: Fixed,
}

impl Accounting {
    /// Applies computed engine output exactly once, leaving the cache intact on failure.
    /// Error cases:
    /// - liabilities, unpaid approvals, or the deposit count overflow
    pub(crate) fn apply(&mut self, command: &Command, events: &[Event]) -> anyhow::Result<()> {
        let next = self.updated(command, events).inspect_err(|err| {
            error!(alert = "checkpoint_accounting_overflow", error = %err, "checkpoint accounting refused engine output");
        })?;
        *self = next;
        Ok(())
    }

    /// Cumulative approvals; authenticated chain payments are subtracted during reconciliation.
    pub(crate) fn unpaid(self) -> Fixed {
        self.unpaid
    }

    /// Computes a checked replacement without exposing a partially applied event batch.
    /// Error cases:
    /// - an aggregate cannot represent the computed output
    fn updated(mut self, command: &Command, events: &[Event]) -> anyhow::Result<Self> {
        let retry = matches!(command, Command::RetryDepositScreening { .. });
        // A receipt's reserve follows its lifecycle event; its public pending or held event does not
        // move it again.
        let receipt = matches!(
            command,
            Command::CustodyDeposit(_)
                | Command::AccessCustodyDeposit { .. }
                | Command::AccessCustodyReplacement { .. }
        );
        for event in events {
            match event {
                Event::DepositPending { .. } | Event::DepositHeld { .. } if receipt => {}
                Event::DepositPending { amount, reason, .. } if !retry && *reason != AccessReject::DepositPending => {
                    self.pending = Fixed::from_raw(
                        self.pending.raw().checked_add(amount.raw()).context("pending collateral overflow")?,
                    );
                    self.liabilities = Fixed::from_raw(
                        self.liabilities.raw().checked_add(amount.raw()).context("checkpoint liabilities overflow")?,
                    );
                }
                Event::DepositHeld { amount, reason, .. }
                    if !matches!(reason, AccessReject::DepositHeld | AccessReject::ProtocolInactive) =>
                {
                    self.held =
                        Fixed::from_raw(self.held.raw().checked_add(amount.raw()).context("held collateral overflow")?);
                    if retry {
                        self.pending_resolved(*amount)?;
                    } else {
                        self.liabilities = Fixed::from_raw(
                            self.liabilities
                                .raw()
                                .checked_add(amount.raw())
                                .context("checkpoint liabilities overflow")?,
                        );
                    }
                }
                Event::Deposited { amount, .. } | Event::HouseCapitalMoved { amount, .. } => {
                    if retry {
                        self.pending_resolved(*amount)?;
                    } else {
                        self.liabilities = Fixed::from_raw(
                            self.liabilities
                                .raw()
                                .checked_add(amount.raw())
                                .context("checkpoint liabilities overflow")?,
                        );
                    }
                }
                Event::WithdrawalApproved { amount, .. } => {
                    self.unpaid = Fixed::from_raw(
                        self.unpaid.raw().checked_add(amount.raw()).context("checkpoint unpaid approvals overflow")?,
                    );
                }
                Event::DistributionAllocated { amount, .. } => {
                    // Allocations change the claim holder, not cumulative custody credits.
                    i128::try_from(amount.0)
                        .ok()
                        .and_then(|units| units.checked_mul(1_000))
                        .context("checkpoint distribution overflow")?;
                }
                Event::CustodyDepositChanged { receipt, status, principal_delta, uncredited } => {
                    self.receipt_changed(command, receipt, status, *principal_delta, *uncredited)?;
                }
                _ => {}
            }
            if evm_disposition(command, event) {
                self.deposits = self.deposits.checked_add(1).context("checkpoint deposit count overflow")?;
            }
        }
        Ok(self)
    }

    /// Applies one receipt transition. Credited principal moves liabilities and the vault deposit
    /// count; an uncredited receipt reserves its principal as pending or held collateral while it is
    /// live, and a retry that credits it turns the pending reserve into cash.
    /// Error cases:
    /// - the event's receipt, status or movement differs from its command's transition
    /// - an aggregate cannot represent the result
    fn receipt_changed(
        &mut self,
        command: &Command,
        receipt: &DepositReceipt,
        status: &DepositStatus,
        principal_delta: Fixed,
        uncredited: Option<AccessReject>,
    ) -> anyhow::Result<()> {
        if let Command::RetryDepositScreening { chain_ref, .. } = command {
            ensure!(*chain_ref == receipt.reference(), "custody accounting receipt differs from command");
            return match uncredited {
                None if principal_delta == receipt.amount => self.pending_resolved(receipt.amount),
                // The hold's own event moves the pending reserve.
                Some(reason) if principal_delta.is_zero() && reason.holds_deposit() => Ok(()),
                _ => anyhow::bail!("custody accounting delta differs from receipt transition"),
            };
        }
        // A live receipt displacing another at its deposit id reverses the stale one in the same
        // command, and activating `V38` resolves surviving duplicates the same way: account for the
        // reversal from the event alone. Every other event must name the command's own receipt.
        let subject_matches = match command {
            Command::CustodyDeposit(update) | Command::AccessCustodyReplacement { update, .. } => {
                let replacement =
                    matches!(&**update, DepositUpdate::Replace { replacement, .. } if replacement == receipt);
                update.subject() == receipt || replacement
            }
            Command::AccessCustodyDeposit { receipt: subject, .. } => subject == receipt,
            _ => false,
        };
        if !subject_matches {
            ensure!(*status == DepositStatus::Reversed, "custody accounting event has no receipt command");
            return self.collision_reversed(receipt, principal_delta, uncredited);
        }
        let update = match command {
            Command::CustodyDeposit(update) | Command::AccessCustodyReplacement { update, .. } => {
                update.as_ref().clone()
            }
            Command::AccessCustodyDeposit { receipt, .. } => DepositUpdate::Credit { receipt: receipt.clone() },
            _ => anyhow::bail!("custody accounting event has no receipt command"),
        };
        let replacement = matches!(&update, DepositUpdate::Replace { replacement, .. } if replacement == receipt);
        ensure!(update.subject() == receipt || replacement, "custody accounting receipt differs from command");
        let (expected, credited) = match update {
            DepositUpdate::Credit { .. } => (DepositStatus::Unfinalised, receipt.amount),
            DepositUpdate::Reinstate { .. } => (DepositStatus::Unfinalised, receipt.amount),
            DepositUpdate::Finalise { .. } => (DepositStatus::Finalised, Fixed::ZERO),
            DepositUpdate::Reverse { .. } => (DepositStatus::Reversed, -receipt.amount),
            DepositUpdate::Replace { .. } if replacement => (DepositStatus::Unfinalised, receipt.amount),
            DepositUpdate::Replace { .. } => (DepositStatus::Reversed, -receipt.amount),
        };
        let adopted = matches!(update, DepositUpdate::Credit { .. }) && principal_delta.is_zero();
        let valid = *status == expected
            && match uncredited {
                None => principal_delta == credited || adopted,
                Some(_) => principal_delta.is_zero(),
            };
        ensure!(valid, "custody accounting delta differs from receipt transition");
        // Credited principal moved cash; an uncredited receipt's live principal is reserved instead.
        let movement = match uncredited {
            None => principal_delta,
            Some(reason) => {
                let reserve = if reason.holds_deposit() { &mut self.held } else { &mut self.pending };
                *reserve = Fixed::from_raw(
                    reserve
                        .raw()
                        .checked_add(credited.raw())
                        .filter(|reserved| *reserved >= 0)
                        .context("uncredited collateral out of range")?,
                );
                credited
            }
        };
        self.liabilities = Fixed::from_raw(
            self.liabilities.raw().checked_add(movement.raw()).context("checkpoint liabilities overflow")?,
        );
        if movement.is_positive() {
            self.deposits = self.deposits.checked_add(1).context("checkpoint deposit count overflow")?;
        } else if movement.is_negative() {
            self.deposits = self.deposits.checked_sub(1).context("checkpoint deposit count underflow")?;
        }
        Ok(())
    }

    /// Applies an automatic collision reversal: the engine reversed a live receipt because another
    /// claimed its deposit id, either inside the displacing command or at `V38` activation.
    /// Error cases:
    /// - the event's status or movement is not a reversal of the receipt it names
    /// - an aggregate cannot represent the result
    fn collision_reversed(
        &mut self,
        receipt: &DepositReceipt,
        principal_delta: Fixed,
        uncredited: Option<AccessReject>,
    ) -> anyhow::Result<()> {
        let credited = -receipt.amount;
        let valid = match uncredited {
            None => principal_delta == credited,
            Some(_) => principal_delta.is_zero(),
        };
        ensure!(valid, "custody accounting delta differs from receipt transition");
        let movement = match uncredited {
            None => principal_delta,
            Some(reason) => {
                let reserve = if reason.holds_deposit() { &mut self.held } else { &mut self.pending };
                *reserve = Fixed::from_raw(
                    reserve
                        .raw()
                        .checked_add(credited.raw())
                        .filter(|reserved| *reserved >= 0)
                        .context("uncredited collateral out of range")?,
                );
                credited
            }
        };
        self.liabilities = Fixed::from_raw(
            self.liabilities.raw().checked_add(movement.raw()).context("checkpoint liabilities overflow")?,
        );
        if movement.is_negative() {
            self.deposits = self.deposits.checked_sub(1).context("checkpoint deposit count underflow")?;
        }
        Ok(())
    }

    /// Removes an existing uncredited reserve after a terminal screening decision.
    /// Error cases:
    /// - the emitted amount exceeds the pending reserve
    fn pending_resolved(&mut self, amount: Fixed) -> anyhow::Result<()> {
        self.pending = Fixed::from_raw(
            self.pending
                .raw()
                .checked_sub(amount.raw())
                .filter(|remaining| *remaining >= 0)
                .context("pending collateral underflow")?,
        );
        Ok(())
    }
}

/// A final vault deposit counts only on its first credited, pending or held disposition.
fn evm_disposition(command: &Command, event: &Event) -> bool {
    match (command, event) {
        (Command::AccessDeposit { deposit, .. }, Event::Deposited { account, amount }) => {
            deposit.chain_ref.starts_with("0x") && deposit.account == Some(*account) && deposit.amount == *amount
        }
        (Command::AccessDeposit { deposit, .. }, Event::HouseCapitalMoved { amount, .. }) => {
            deposit.chain_ref.starts_with("0x") && deposit.account.is_none() && deposit.amount == *amount
        }
        (
            Command::AccessDeposit { deposit: AccessDeposit { chain_ref, amount, .. }, .. }
            | Command::Deposit { chain_ref, amount, .. }
            | Command::HouseCapital { chain_ref, amount },
            Event::DepositPending { chain_ref: disposed, amount: disposed_amount, reason, .. }
            | Event::DepositHeld { chain_ref: disposed, amount: disposed_amount, reason, .. },
        ) => {
            chain_ref.starts_with("0x")
                && chain_ref == disposed
                && amount == disposed_amount
                && !matches!(
                    reason,
                    AccessReject::DepositPending | AccessReject::DepositHeld | AccessReject::ProtocolInactive
                )
        }
        (Command::Deposit { account, amount, chain_ref }, Event::Deposited { account: credited, amount: moved }) => {
            chain_ref.starts_with("0x") && account == credited && amount == moved
        }
        (Command::HouseCapital { amount, chain_ref }, Event::HouseCapitalMoved { amount: moved, .. }) => {
            chain_ref.starts_with("0x") && amount == moved
        }
        _ => false,
    }
}

#[cfg(test)]
mod access_tests;
#[cfg(test)]
pub(crate) mod tests;
