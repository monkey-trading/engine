//! Fixed-size custody accounting derived from authenticated engine output, never host metadata.

use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::deposits::{DepositStatus, DepositUpdate};
use engine_core::event::Event;
use serde::{Deserialize, Serialize};
use tracing::error;

/// Cash, unpaid approvals, and allocated distributions equal cumulative custody credits.
/// Allocations transfer house cash to staker claims without changing the credited deposit total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Accounting {
    pub(super) liabilities: Fixed,
    pub(super) deposits: u64,
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
        for event in events {
            match event {
                Event::Deposited { amount, .. } | Event::HouseCapitalMoved { amount, .. } => {
                    self.liabilities = Fixed::from_raw(
                        self.liabilities.raw().checked_add(amount.raw()).context("checkpoint liabilities overflow")?,
                    );
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
                Event::CustodyDepositChanged { receipt, status, principal_delta } => {
                    let Command::CustodyDeposit(update) = command else {
                        anyhow::bail!("custody accounting event has no receipt command");
                    };
                    let replacement =
                        matches!(update.as_ref(), DepositUpdate::Replace { replacement, .. } if replacement == receipt);
                    ensure!(
                        update.subject() == receipt || replacement,
                        "custody accounting receipt differs from command"
                    );
                    let valid = match update.as_ref() {
                        DepositUpdate::Credit { .. } => {
                            *status == DepositStatus::Unfinalised
                                && (principal_delta.is_zero() || *principal_delta == receipt.amount)
                        }
                        DepositUpdate::Reinstate { .. } => {
                            *status == DepositStatus::Unfinalised && *principal_delta == receipt.amount
                        }
                        DepositUpdate::Finalise { .. } => {
                            *status == DepositStatus::Finalised && principal_delta.is_zero()
                        }
                        DepositUpdate::Reverse { .. } => {
                            *status == DepositStatus::Reversed && *principal_delta == -receipt.amount
                        }
                        DepositUpdate::Replace { .. } if replacement => {
                            *status == DepositStatus::Unfinalised && *principal_delta == receipt.amount
                        }
                        DepositUpdate::Replace { .. } => {
                            *status == DepositStatus::Reversed && *principal_delta == -receipt.amount
                        }
                    };
                    ensure!(valid, "custody accounting delta differs from receipt transition");
                    self.liabilities = Fixed::from_raw(
                        self.liabilities
                            .raw()
                            .checked_add(principal_delta.raw())
                            .context("checkpoint liabilities overflow")?,
                    );
                    if principal_delta.is_positive() {
                        self.deposits = self.deposits.checked_add(1).context("checkpoint deposit count overflow")?;
                    } else if principal_delta.is_negative() {
                        self.deposits = self.deposits.checked_sub(1).context("checkpoint deposit count underflow")?;
                    }
                }
                _ => {}
            }
            if evm_credit(command, event) {
                self.deposits = self.deposits.checked_add(1).context("checkpoint deposit count overflow")?;
            }
        }
        Ok(self)
    }
}

/// A reference counts only when this command produced its matching successful credit.
fn evm_credit(command: &Command, event: &Event) -> bool {
    match (command, event) {
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
pub(crate) mod tests;
