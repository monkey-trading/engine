//! Fixed-size custody accounting derived from authenticated engine output, never host metadata.

use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::access::{AccessDeposit, AccessReject};
use engine_core::command::Command;
use engine_core::deposits::{DepositStatus, DepositUpdate};
use engine_core::event::Event;
use serde::{Deserialize, Serialize};
use tracing::error;

/// Cash, unpaid approvals, pending and held collateral equal processed custody funding.
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
        for event in events {
            match event {
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
            if evm_disposition(command, event) {
                self.deposits = self.deposits.checked_add(1).context("checkpoint deposit count overflow")?;
            }
        }
        Ok(self)
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
