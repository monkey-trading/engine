//! From `V83`, a reorganisation's receipt moves applied as one command.
//!
//! A reorganisation above the finalised frontier can include each credited deposit again, unchanged,
//! at another vault deposit id: another deposit taking an earlier id shifts every later one up. Moved
//! one at a time, the first receipt would land on the id the next one's live receipt still holds, and
//! that receipt, though canonical, would be reversed through the cash path and its account closed out.
//! Here every orphan leaves its id before any replacement takes one, so each deposit moves as a
//! `Replace` does, with its cash and principal lock netting out, and a replacement may take an id only
//! from receipts that move away in the same command. A receipt the chain dropped reverses by its own
//! `Reverse`, which the custody watcher sequences before the moves.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::access::{AccessEvidence, CreditScreening, UncreditedReceipt};
use crate::deposits::{DepositReceipt, DepositReject, DepositStatus, DepositUpdate};
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;
use crate::types::AccountId;

/// A live receipt whose transaction-local deposit is canonical again in another block, possibly at
/// another vault id, with the same beneficiary and amount: a `DepositUpdate::Replace` with the
/// replacement's vault caller and screening, as `Command::AccessCustodyReplacement` carries them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptMove {
    pub receipt: DepositReceipt,
    /// The orphan's revision the watcher read.
    pub observed_at_seq: u64,
    pub replacement: DepositReceipt,
    /// The replacement's reversal sequence, when that exact receipt was credited and reversed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_reversed_at_seq: Option<u64>,
    /// The replacement's vault caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<AccountId>,
    /// The replacement's screening; none from a node that does not screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<AccessEvidence>,
}

impl ReceiptMove {
    /// The move as the single `DepositUpdate::Replace` it applies as.
    pub fn update(&self) -> DepositUpdate {
        DepositUpdate::Replace {
            receipt: self.receipt.clone(),
            observed_at_seq: self.observed_at_seq,
            replacement: self.replacement.clone(),
            replacement_reversed_at_seq: self.replacement_reversed_at_seq,
        }
    }
}

/// A move that has not applied yet, with what keeps the orphan's and the replacement's principal out of
/// cash.
struct Planned<'a> {
    move_: &'a ReceiptMove,
    old: Option<UncreditedReceipt>,
    new: Option<UncreditedReceipt>,
}

impl Engine {
    /// Applies a reorganisation's moves together, or none of them, reporting a refusal against the move
    /// that failed its checks.
    pub(crate) fn reorganise_deposits(&mut self, moves: &[ReceiptMove], events: &mut Vec<Event>) {
        let Some(first) = moves.first() else {
            events.push(Event::AdminRejected { reason: "a reorganisation moves at least one receipt".into() });
            return;
        };
        let result = if self.protocol_version < ProtocolVersion::V83 {
            Err((first, DepositReject::ProtocolNotActive))
        } else {
            self.apply_reorganisation(moves, events)
        };
        if let Err((move_, reason)) = result {
            events.push(Event::CustodyDepositRejected { receipt: move_.receipt.clone(), reason });
        }
    }

    /// Checks every move, then reverses every orphan and records every replacement, moving cash only
    /// where screening changes a deposit's credit. A move that has already applied is skipped.
    /// Error cases:
    /// - a move fails a `Replace`'s checks (`check_replacement`), from `V88` its id below the finalised
    ///   frontier's deposit count among them
    /// - two moves name the same orphan, replacement, vault id or transaction-local deposit
    /// - a replacement's vault id is held by a live receipt that does not move in this command
    /// - a change in credited principal cannot be represented
    fn apply_reorganisation<'a>(
        &mut self,
        moves: &'a [ReceiptMove],
        events: &mut Vec<Event>,
    ) -> Result<(), (&'a ReceiptMove, DepositReject)> {
        self.ensure_hot_indexes();
        let refused = |move_: &'a ReceiptMove, reason| (move_, reason);
        let mut pending = Vec::new();
        let (mut orphans, mut replacements, mut ids, mut transactions) =
            (BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for move_ in moves {
            self.check_subject(&move_.receipt).map_err(|reason| refused(move_, reason))?;
            let (orphan, replacement) = (&move_.receipt, &move_.replacement);
            let applies = self
                .check_replacement(orphan, move_.observed_at_seq, replacement, move_.replacement_reversed_at_seq)
                .map_err(|reason| refused(move_, reason))?;
            let distinct = orphans.insert(orphan.reference())
                && replacements.insert(replacement.reference())
                && ids.insert(replacement.deposit_id)
                && transactions.insert(orphan.transaction_reference());
            if !distinct {
                return Err(refused(move_, DepositReject::InvalidReceipt));
            }
            if applies {
                pending.push(move_);
            }
        }
        let indexes = self.hot_indexes.as_ref().expect("indexes initialised");
        if let Some(move_) = pending.iter().find(|move_| {
            let replacement = &move_.replacement;
            indexes.conflicts_at(replacement.deposit_id, &replacement.reference()).iter().any(|r| !orphans.contains(r))
        }) {
            return Err(refused(move_, DepositReject::ConflictingReceipt));
        }
        let planned: Vec<Planned<'_>> = pending
            .into_iter()
            .map(|move_| {
                let old = self.deposits.get(&move_.receipt.reference()).expect("checked above").uncredited;
                let screening = CreditScreening { payer: move_.payer, evidence: move_.evidence.as_ref() };
                let new = self.replacement_refusal(&move_.receipt, &move_.replacement, screening);
                Planned { move_, old, new }
            })
            .collect();
        // Cash first, so a refusal leaves every balance as it was.
        let mut moved: Vec<(&DepositReceipt, bool)> = Vec::new();
        for plan in planned.iter().filter(|plan| plan.old.is_none() != plan.new.is_none()) {
            let (replacement, credit) = (&plan.move_.replacement, plan.new.is_none());
            if let Err(reason) = self.move_deposit_cash(replacement, credit, true) {
                for (receipt, credit) in moved.into_iter().rev() {
                    self.move_deposit_cash(receipt, !credit, true).expect("restores the cash moved above");
                }
                return Err(refused(plan.move_, reason));
            }
            moved.push((replacement, credit));
        }
        // Every orphan leaves its id before any replacement takes one.
        for plan in &planned {
            let orphan = &plan.move_.receipt;
            let delta = if plan.old.is_none() { -orphan.amount } else { Fixed::ZERO };
            self.record_deposit(orphan, DepositStatus::Reversed, delta, plan.old, events)
                .expect("a reversal displaces no receipt");
            if let Some(old) = plan.old {
                self.track_uncredited(orphan, old, false);
            }
        }
        for plan in &planned {
            let replacement = &plan.move_.replacement;
            let delta = if plan.new.is_none() { replacement.amount } else { Fixed::ZERO };
            self.record_deposit(replacement, DepositStatus::Unfinalised, delta, plan.new, events)
                .expect("checked above: only receipts that moved away held the id");
            self.deposits.get_mut(&replacement.reference()).expect("replacement recorded").payer = plan.move_.payer;
            if let Some(new) = plan.new {
                self.track_uncredited(replacement, new, true);
                self.announce_uncredited(replacement, new, events);
            }
        }
        // Each account that lost a credit keeps every such principal before its one close-out, so the
        // write-off they leave counts against all of them.
        let mut debited = BTreeSet::new();
        for plan in &planned {
            match (plan.old, plan.new) {
                (Some(_), None) => self.repay_write_off(&plan.move_.replacement, events),
                (None, Some(_)) => debited.extend(self.keep_reversed_principal(&plan.move_.replacement)),
                _ => {}
            }
        }
        for account in debited {
            self.close_out_after_balance_change(account, events);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
