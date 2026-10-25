//! Legacy credits carry no block or finality proof; activation locks their principal without cash.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::command::Command;
use crate::deposits::{DepositBeneficiary, DepositReceipt, DepositReject, DepositStatus};
use crate::engine::Engine;
use crate::event::Event;

/// A successful legacy EVM credit, reconstructed from its command and replayed event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyDeposit {
    pub chain_ref: String,
    pub beneficiary: DepositBeneficiary,
    pub amount: Fixed,
}

/// Parses exactly the reference emitted by the historical EVM adapter, excluding mock references.
pub fn legacy_chain_reference(reference: &str) -> Option<(B256, u64)> {
    let (hash, index) = reference.split_once(':')?;
    let (hash, index) = (hash.parse::<B256>().ok()?, index.parse::<u64>().ok()?);
    (format!("{hash}:{index}") == reference).then_some((hash, index))
}

impl LegacyDeposit {
    /// Recovers credit provenance only from a successful command's matching replayed event.
    pub fn from_command(command: &Command, events: &[Event]) -> Option<Self> {
        let (chain_ref, amount, beneficiary, credited) = match command {
            Command::Deposit { account, amount, chain_ref } => (
                chain_ref,
                *amount,
                DepositBeneficiary::Trader { account: *account },
                events
                    .iter()
                    .any(|e| matches!(e, Event::Deposited { account: a, amount: n } if a == account && n == amount)),
            ),
            Command::HouseCapital { amount, chain_ref } => (
                chain_ref,
                *amount,
                DepositBeneficiary::House,
                events.iter().any(|e| matches!(e, Event::HouseCapitalMoved { amount: n, .. } if n == amount)),
            ),
            _ => return None,
        };
        (credited && amount.is_positive() && legacy_chain_reference(chain_ref).is_some()).then(|| Self {
            chain_ref: chain_ref.clone(),
            beneficiary,
            amount,
        })
    }
}

impl Engine {
    /// An unresolved legacy credit; its principal remains locked without an exact receipt proof.
    pub fn unresolved_legacy_deposit(&self) -> Option<&LegacyDeposit> {
        self.legacy_deposits.iter().next().map(|(_, deposit)| deposit)
    }

    /// The unresolved principal for an exact historical reference, without assuming finality.
    pub fn unresolved_legacy_credit(&self, reference: &str) -> Option<&LegacyDeposit> {
        self.legacy_deposits.get(reference)
    }

    /// Whether the current canonical exact receipt independently finalised an old credit.
    pub fn legacy_credit_finalised(&self, legacy: &LegacyDeposit) -> bool {
        let Some((transaction, index)) = legacy_chain_reference(&legacy.chain_ref) else { return false };
        let prefix = format!("{transaction}:");
        self.deposit_transactions
            .range(prefix.clone()..)
            .take_while(|(reference, _)| reference.starts_with(&prefix))
            .filter_map(|(_, reference)| self.deposits.get(reference))
            .any(|record| {
                record.status == DepositStatus::Finalised
                    && record.receipt.tx_hash == transaction
                    && record.receipt.log_index == index
                    && record.receipt.beneficiary == legacy.beneficiary
                    && record.receipt.amount == legacy.amount
            })
    }

    /// Unresolved historical references for a transaction, excluding already adopted receipts.
    pub fn unresolved_legacy_references(&self, prefix: &str) -> impl Iterator<Item = &str> {
        self.legacy_deposits
            .range(prefix.to_owned()..)
            .take_while(move |(r, _)| r.starts_with(prefix))
            .map(|(r, _)| r.as_str())
    }

    /// Locks every recorded legacy EVM credit atomically, without applying cash a second time.
    /// Error cases:
    /// - missing, duplicate, unknown or invalid legacy credits, or a principal sum that overflows
    pub(crate) fn lock_legacy_deposits(&mut self, deposits: &[LegacyDeposit]) -> Result<(), String> {
        if !self.legacy_deposits.is_empty() || !self.deposits.is_empty() {
            return Err("custody receipt migration has already started".into());
        }
        let expected: BTreeSet<_> = self.credited_refs.iter().filter(|r| legacy_chain_reference(r).is_some()).collect();
        let actual: BTreeSet<_> = deposits.iter().map(|d| &d.chain_ref).collect();
        if actual.len() != deposits.len() || actual != expected {
            return Err("legacy custody credits must cover every recorded EVM reference exactly once".into());
        }
        let (mut accounts, mut house) = (BTreeMap::new(), self.unfinalised_house_deposits.raw());
        for deposit in deposits {
            if !deposit.amount.is_valid_input() {
                return Err("legacy custody credit has an invalid amount".into());
            }
            let locked = match deposit.beneficiary {
                DepositBeneficiary::Trader { account } => {
                    let state = self.accounts.get(&account).ok_or("legacy custody account does not exist")?;
                    accounts.entry(account).or_insert(state.unfinalised_deposits.raw())
                }
                DepositBeneficiary::House => &mut house,
            };
            *locked = locked.checked_add(deposit.amount.raw()).ok_or("legacy custody principal overflow")?;
        }
        for (account, locked) in accounts {
            self.accounts.get_mut(&account).expect("validated legacy account exists").unfinalised_deposits =
                Fixed::from_raw(locked);
        }
        self.unfinalised_house_deposits = Fixed::from_raw(house);
        for deposit in deposits {
            self.legacy_deposits.insert(deposit.chain_ref.clone(), deposit.clone());
        }
        Ok(())
    }

    /// Attaches a canonical receipt to already locked legacy principal, without cash or release.
    /// Error cases:
    /// - changed economics, missing migration evidence or an ambiguous moved legacy transaction
    pub(super) fn adopt_legacy_deposit(&mut self, receipt: &DepositReceipt) -> Result<bool, DepositReject> {
        let reference = format!("{}:{}", receipt.tx_hash, receipt.log_index);
        if let Some(legacy) = self.legacy_deposits.get(&reference) {
            if legacy.amount != receipt.amount || legacy.beneficiary != receipt.beneficiary {
                return Err(DepositReject::ConflictingReceipt);
            }
            self.check_transaction_available(receipt)?;
            self.legacy_deposits.remove(&reference);
            return Ok(true);
        }
        let prefix = format!("{}:", receipt.tx_hash);
        if self.legacy_deposits.range(prefix.clone()..).next().is_some_and(|(r, _)| r.starts_with(&prefix))
            || (self.is_credited(&reference)
                && !self.deposit_transactions.contains_key(&receipt.transaction_reference()))
        {
            return Err(DepositReject::ConflictingReceipt);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests;
