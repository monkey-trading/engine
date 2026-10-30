//! Sequencer-confirmed custody receipts, principal locks, and canonical reversals.

use std::ops::Bound;

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::access::{CreditScreening, UncreditedReceipt};
use crate::engine::Engine;
use crate::event::Event;
use crate::types::AccountId;
mod cash;
mod conflicts;
mod legacy;
mod replacement;
pub use legacy::{LegacyDeposit, legacy_chain_reference};

/// The recipient of a vault deposit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DepositBeneficiary {
    Trader { account: AccountId },
    House,
}

/// An exact canonical vault event. The node verifies its chain and vault before sequencing it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepositReceipt {
    pub block: u64,
    pub block_hash: B256,
    pub tx_hash: B256,
    pub log_index: u64,
    /// Ordinal among this vault's deposit events in the transaction, independent of global indices.
    pub transaction_deposit_index: u64,
    pub deposit_id: u64,
    pub beneficiary: DepositBeneficiary,
    pub amount: Fixed,
}

/// The recorded disposition of one exact receipt, retained across retries and restarts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DepositStatus {
    Unfinalised,
    Finalised,
    Reversed,
}

/// The replayed finalised scan frontier; the host cursor cannot advance it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustodyCheckpoint {
    pub block: u64,
    pub block_hash: B256,
    pub deposit_count: u64,
}

/// A receipt and its current lifecycle state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedDeposit {
    pub receipt: DepositReceipt,
    pub status: DepositStatus,
    pub updated_at_seq: u64,
    /// From `V24`, why access screening keeps the receipt's principal out of cash. It survives a
    /// reversal, so a reinstatement or replacement keeps the receipt uncredited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncredited: Option<UncreditedReceipt>,
    /// Canonical vault caller authenticated by screening from V26.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<AccountId>,
}

/// Custody observations admitted by the RPC quorum and persisted in the public log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum DepositUpdate {
    Credit {
        receipt: DepositReceipt,
    },
    Finalise {
        receipt: DepositReceipt,
        observed_at_seq: u64,
    },
    Reverse {
        receipt: DepositReceipt,
        observed_at_seq: u64,
    },
    /// Reappearance of an exact reversed block, tied to that reversal's sequence.
    Reinstate {
        receipt: DepositReceipt,
        reversed_at_seq: u64,
    },
    /// A live receipt whose transaction-local deposit is canonical again in another block, with the
    /// same beneficiary and amount. One step reverses it and credits the replacement, so cash and
    /// the principal lock net out.
    Replace {
        receipt: DepositReceipt,
        observed_at_seq: u64,
        replacement: DepositReceipt,
        /// The replacement's reversal sequence, when that exact receipt was credited and reversed.
        replacement_reversed_at_seq: Option<u64>,
    },
}

/// A custody transition refused without changing its receipt, principal lock, or cash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DepositReject {
    ProtocolNotActive,
    InvalidReceipt,
    InvalidAmount,
    ArithmeticOverflow,
    UnknownReceipt,
    ConflictingReceipt,
    SettledReceipt,
}

impl DepositReceipt {
    /// A height-ordered exact identity, including the block hash and receipt's log index.
    pub fn reference(&self) -> String {
        format!("{:020}:{}:{}:{}", self.block, self.block_hash, self.tx_hash, self.log_index)
    }

    /// Stable transaction-local identity; a global vault id may change on reinclusion.
    pub fn transaction_reference(&self) -> String {
        format!("{}:{}", self.tx_hash, self.transaction_deposit_index)
    }
}

impl DepositUpdate {
    /// The exact receipt the transition acts on.
    pub fn subject(&self) -> &DepositReceipt {
        match self {
            Self::Credit { receipt }
            | Self::Finalise { receipt, .. }
            | Self::Reverse { receipt, .. }
            | Self::Reinstate { receipt, .. }
            | Self::Replace { receipt, .. } => receipt,
        }
    }
}

impl Engine {
    /// Whether sequencer-confirmed deposits and principal locks are active.
    pub fn deposit_finality_enabled(&self) -> bool {
        self.protocol_version.supports_deposit_finality()
    }

    /// The finalised custody frontier established by sequenced observations.
    pub fn custody_checkpoint(&self) -> Option<CustodyCheckpoint> {
        self.custody_checkpoint
    }

    /// Canonical coverage at this prefix, without scanning historical receipts or legacy references.
    /// No value is available before a sequenced finalised frontier and rebuilt indexes exist.
    pub fn custody_deposit_coverage(&self) -> Option<u64> {
        let checkpoint = self.custody_checkpoint?;
        let above = u64::try_from(self.hot_indexes.as_ref()?.receipts_above_frontier.len()).ok()?;
        checkpoint.deposit_count.checked_add(above)
    }

    /// Records the finalised frontier after every receipt below it has settled.
    pub(crate) fn advance_custody_checkpoint(&mut self, checkpoint: CustodyCheckpoint, events: &mut Vec<Event>) {
        self.ensure_hot_indexes();
        let pending_below =
            self.hot_indexes.as_ref().expect("indexes initialised").pending_deposits.first().is_some_and(|reference| {
                self.deposits.get(reference).expect("indexed receipt exists").receipt.block <= checkpoint.block
            });
        let invalid = !self.deposit_finality_enabled()
            || checkpoint.block == u64::MAX
            || checkpoint.block_hash == B256::ZERO
            || self.custody_checkpoint.is_some_and(|old| {
                checkpoint.block < old.block
                    || checkpoint.deposit_count < old.deposit_count
                    || (checkpoint.block == old.block && checkpoint != old)
            })
            || pending_below;
        if invalid {
            events.push(Event::AdminRejected { reason: "invalid custody finalised frontier".into() });
            return;
        }
        self.hot_indexes.as_mut().expect("indexes initialised").cover_deposits_through(checkpoint.block);
        self.custody_checkpoint = Some(checkpoint);
    }

    /// The recorded exact receipt, including terminal dispositions.
    pub fn recorded_deposit(&self, reference: &str) -> Option<&RecordedDeposit> {
        self.deposits.get(reference)
    }

    /// Unfinalised receipts in a bounded block window, in deterministic identity order.
    pub fn unfinalised_deposits(&self, from: u64, to: u64) -> Vec<DepositReceipt> {
        self.unfinalised_deposit_records(from, to).into_iter().map(|d| d.receipt).collect()
    }

    /// Pending observations with their sequenced revisions for conditional transitions.
    pub fn unfinalised_deposit_records(&self, from: u64, to: u64) -> Vec<RecordedDeposit> {
        if from > to {
            return Vec::new();
        }
        if let Some(indexes) = &self.hot_indexes {
            return indexes
                .pending_deposits
                .range(format!("{from:020}:")..=format!("{to:020}:~"))
                .map(|reference| self.deposits.get(reference).expect("indexed pending receipt exists").clone())
                .collect();
        }
        self.deposits
            .range(format!("{from:020}:")..=format!("{to:020}:~"))
            .filter(|(_, deposit)| deposit.status == DepositStatus::Unfinalised)
            .map(|(_, deposit)| deposit.clone())
            .collect()
    }

    /// Applies an admitted custody observation, emitting a rejection for inconsistent input. From
    /// `V24` a new receipt is credited only on `screening` that passes the access rules.
    pub(crate) fn update_deposit(
        &mut self,
        update: &DepositUpdate,
        screening: CreditScreening<'_>,
        events: &mut Vec<Event>,
    ) {
        let result = if self.deposit_finality_enabled() {
            self.apply_deposit_update(update, screening, events)
        } else {
            Err(DepositReject::ProtocolNotActive)
        };
        if let Err(reason) = result {
            events.push(Event::CustodyDepositRejected { receipt: update.subject().clone(), reason });
        }
    }

    /// Changes one receipt atomically after validating its entire transition.
    /// Error cases:
    /// - invalid amounts, arithmetic overflow, or an inconsistent receipt or transition
    fn apply_deposit_update(
        &mut self,
        update: &DepositUpdate,
        screening: CreditScreening<'_>,
        events: &mut Vec<Event>,
    ) -> Result<(), DepositReject> {
        let receipt = update.subject();
        let reference = receipt.reference();
        if receipt.block_hash == B256::ZERO || receipt.tx_hash == B256::ZERO || receipt.block == u64::MAX {
            return Err(DepositReject::InvalidReceipt);
        }
        if !receipt.amount.is_valid_input() {
            return Err(DepositReject::InvalidAmount);
        }
        let recorded = self.deposits.get(&reference);
        if recorded.is_some_and(|recorded| recorded.receipt != *receipt) {
            return Err(DepositReject::ConflictingReceipt);
        }
        if let DepositUpdate::Replace { observed_at_seq, replacement, replacement_reversed_at_seq, .. } = update {
            return self.replace_deposit(
                receipt,
                *observed_at_seq,
                replacement,
                *replacement_reversed_at_seq,
                screening,
                events,
            );
        }
        if let DepositUpdate::Reinstate { reversed_at_seq, .. } = update {
            if self.custody_checkpoint.is_some_and(|checkpoint| receipt.block <= checkpoint.block) {
                return Err(DepositReject::SettledReceipt);
            }
            let recorded = recorded.ok_or(DepositReject::UnknownReceipt)?;
            if recorded.updated_at_seq > *reversed_at_seq && recorded.status != DepositStatus::Reversed {
                return Ok(());
            }
            if recorded.status != DepositStatus::Reversed || recorded.updated_at_seq != *reversed_at_seq {
                return Err(DepositReject::SettledReceipt);
            }
            let uncredited = self.terminal_receipt_hold(receipt).or(recorded.uncredited);
            self.check_transaction_available(receipt)?;
            self.reverse_conflicting_receipts(receipt, events)?;
            if let Some(uncredited) = uncredited {
                self.record_deposit(receipt, DepositStatus::Unfinalised, Fixed::ZERO, Some(uncredited), events)?;
                self.track_uncredited(receipt, uncredited, true);
                return Ok(());
            }
            self.move_deposit_cash(receipt, true, true)?;
            self.record_deposit(receipt, DepositStatus::Unfinalised, receipt.amount, None, events)?;
            return Ok(());
        }
        if let DepositUpdate::Credit { .. } = update {
            if recorded.is_some() {
                return Ok(());
            }
            if self.custody_checkpoint.is_some_and(|checkpoint| receipt.block <= checkpoint.block) {
                return Err(DepositReject::SettledReceipt);
            }
            self.reverse_conflicting_receipts(receipt, events)?;
            if self.adopt_legacy_deposit(receipt)? {
                self.record_deposit(receipt, DepositStatus::Unfinalised, Fixed::ZERO, None, events)?;
                return Ok(());
            }
            self.check_transaction_available(receipt)?;
            if let Some(uncredited) = self.receipt_refusal(receipt, screening) {
                self.record_deposit(receipt, DepositStatus::Unfinalised, Fixed::ZERO, Some(uncredited), events)?;
                self.track_uncredited(receipt, uncredited, true);
                self.announce_uncredited(receipt, uncredited, events);
                return Ok(());
            }
            self.move_deposit_cash(receipt, true, true)?;
            self.record_deposit(receipt, DepositStatus::Unfinalised, receipt.amount, None, events)?;
            return Ok(());
        }
        let recorded = recorded.ok_or(DepositReject::UnknownReceipt)?;
        let uncredited = recorded.uncredited;
        let status = &recorded.status;
        let observed_at_seq = match update {
            DepositUpdate::Finalise { observed_at_seq, .. } | DepositUpdate::Reverse { observed_at_seq, .. } => {
                *observed_at_seq
            }
            _ => unreachable!("credit, reinstatement and replacement handled above"),
        };
        match update {
            DepositUpdate::Finalise { .. } if *status == DepositStatus::Finalised => Ok(()),
            DepositUpdate::Reverse { .. } if *status == DepositStatus::Reversed => Ok(()),
            _ if *status != DepositStatus::Unfinalised || observed_at_seq != recorded.updated_at_seq => {
                Err(DepositReject::SettledReceipt)
            }
            DepositUpdate::Finalise { .. } => {
                if uncredited.is_none() {
                    self.unlock_deposit(receipt);
                }
                self.record_deposit(receipt, DepositStatus::Finalised, Fixed::ZERO, uncredited, events)?;
                Ok(())
            }
            DepositUpdate::Reverse { .. } if let Some(uncredited) = uncredited => {
                self.record_deposit(receipt, DepositStatus::Reversed, Fixed::ZERO, Some(uncredited), events)?;
                self.track_uncredited(receipt, uncredited, false);
                Ok(())
            }
            DepositUpdate::Reverse { .. } => {
                self.move_deposit_cash(receipt, false, true)?;
                self.record_deposit(receipt, DepositStatus::Reversed, -receipt.amount, None, events)?;
                if let DepositBeneficiary::Trader { account } = receipt.beneficiary {
                    self.close_out_after_balance_change(account, events);
                }
                Ok(())
            }
            DepositUpdate::Credit { .. } | DepositUpdate::Reinstate { .. } | DepositUpdate::Replace { .. } => {
                unreachable!("credit handled before checking an existing receipt")
            }
        }
    }

    /// Refuses a second active receipt for the same transaction-local deposit.
    /// Error cases:
    /// - another receipt for that deposit remains credited
    fn check_transaction_available(&self, receipt: &DepositReceipt) -> Result<(), DepositReject> {
        if let Some(reference) = self.deposit_transactions.get(&receipt.transaction_reference())
            && self.deposits.get(reference).is_some_and(|d| d.status != DepositStatus::Reversed)
        {
            return Err(DepositReject::ConflictingReceipt);
        }
        Ok(())
    }

    /// Records a lifecycle transition and its exact receipt in both state and events.
    /// Error cases:
    /// - a different live receipt already holds the deposit's id and cannot be reversed: only one
    ///   live receipt may hold a vault deposit id, so a reorganisation's replacement displaces the
    ///   stale receipt before the new one records
    pub(crate) fn record_deposit(
        &mut self,
        receipt: &DepositReceipt,
        status: DepositStatus,
        principal_delta: Fixed,
        uncredited: Option<UncreditedReceipt>,
        events: &mut Vec<Event>,
    ) -> Result<(), DepositReject> {
        self.ensure_hot_indexes();
        if status != DepositStatus::Reversed {
            self.reverse_conflicting_receipts(receipt, events)?;
        }
        let indexes = self.hot_indexes.as_mut().expect("indexes initialised");
        if self.protocol_version >= crate::protocol::ProtocolVersion::V26
            && let Some(held) = uncredited.filter(|held| held.reason.holds_deposit())
        {
            indexes.terminal_deposit_holds.entry(receipt.transaction_reference()).or_insert(held);
        }
        let pending = &mut indexes.pending_deposits;
        if status == DepositStatus::Unfinalised {
            pending.insert(receipt.reference());
        } else {
            pending.remove(&receipt.reference());
        }
        if status != DepositStatus::Reversed
            && self.custody_checkpoint.is_none_or(|checkpoint| receipt.block > checkpoint.block)
        {
            indexes.add_above_frontier(receipt.reference(), receipt.deposit_id, receipt.amount);
        } else {
            indexes.remove_above_frontier(&receipt.reference());
        }
        self.deposit_transactions.insert(receipt.transaction_reference(), receipt.reference());
        let payer = self.deposits.get(&receipt.reference()).and_then(|recorded| recorded.payer);
        self.deposits.insert(
            receipt.reference(),
            RecordedDeposit {
                receipt: receipt.clone(),
                status: status.clone(),
                updated_at_seq: self.last_seq.expect("custody updates are sequenced"),
                uncredited,
                payer,
            },
        );
        events.push(Event::CustodyDepositChanged {
            receipt: receipt.clone(),
            status,
            principal_delta,
            uncredited: uncredited.map(|uncredited| uncredited.reason),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;

impl Engine {
    /// Whether the custody reference `chain_ref` has been credited.
    pub fn is_credited(&self, chain_ref: &str) -> bool {
        self.credited_refs.contains(chain_ref)
    }

    /// The credited custody references that start with `prefix`, in order.
    pub fn credited_refs_with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        let from = (Bound::Included(prefix), Bound::Unbounded);
        self.credited_refs.range::<str, _>(from).map(String::as_str).take_while(move |r| r.starts_with(prefix))
    }
}
