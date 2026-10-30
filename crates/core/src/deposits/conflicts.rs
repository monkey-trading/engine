//! One-live-receipt-per-deposit-id resolution for the exit's id-keyed refunds.

use crate::Fixed;
use crate::deposits::{DepositBeneficiary, DepositReceipt, DepositReject, DepositStatus};
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;

impl Engine {
    /// Reverses every other live receipt above the finalised frontier at `receipt`'s deposit id, so
    /// the new canonical receipt holds the id alone. From `V36` the exit refunds pending deposits
    /// by id, which two live receipts at one id would alias. A settled (finalised) conflict refuses
    /// the new receipt: finality is final, and the custody adapter must resolve the collision.
    /// Error cases:
    /// - a conflicting receipt is finalised, or its credited principal can no longer be removed
    pub(super) fn reverse_conflicting_receipts(
        &mut self,
        receipt: &DepositReceipt,
        exclude: Option<&str>,
        events: &mut Vec<Event>,
    ) -> Result<(), DepositReject> {
        if self.protocol_version < ProtocolVersion::V36
            || self.custody_checkpoint.is_some_and(|checkpoint| receipt.block <= checkpoint.block)
        {
            return Ok(());
        }
        let conflicts = self
            .hot_indexes
            .as_ref()
            .expect("indexes initialised")
            .conflicts_at(receipt.deposit_id, &receipt.reference());
        for reference in conflicts {
            // A replacement's orphan yields to its replacement below, not to the sweep: reversing
            // it here would unwind its cash a second time.
            if exclude.is_some_and(|excluded| excluded == reference) {
                continue;
            }
            self.reverse_live_receipt(&reference, events)?;
        }
        Ok(())
    }

    /// Activation-time resolution of duplicate live receipts surviving from before `V36`: at each
    /// vault deposit id above the finalised frontier the earliest-recorded live receipt holds the
    /// id, and the rest reverse.
    /// Error cases:
    /// - a duplicate is finalised, or a duplicate's credited principal can no longer be removed
    pub(crate) fn reverse_duplicate_live_receipts(&mut self, events: &mut Vec<Event>) -> Result<(), String> {
        self.ensure_hot_indexes();
        let duplicates: Vec<Vec<String>> = self
            .hot_indexes
            .as_ref()
            .expect("indexes initialised")
            .above_frontier_by_id
            .values()
            .filter(|references| references.len() > 1)
            .map(|references| references.iter().cloned().collect())
            .collect();
        for mut references in duplicates {
            references.sort_by_key(|reference| {
                (
                    self.deposits.get(reference).expect("an indexed receipt is recorded").updated_at_seq,
                    reference.clone(),
                )
            });
            for reference in references.iter().skip(1) {
                self.reverse_live_receipt(reference, events)
                    .map_err(|reason| format!("live receipts duplicate a deposit id ({reference}: {reason:?})"))?;
            }
        }
        Ok(())
    }

    /// Reverses the live recorded receipt at `reference`, unwinding its credit or hold.
    /// Error cases:
    /// - the receipt is finalised, or its credited principal can no longer be removed
    fn reverse_live_receipt(&mut self, reference: &str, events: &mut Vec<Event>) -> Result<(), DepositReject> {
        let recorded = self.deposits.get(reference).expect("an indexed receipt is recorded").clone();
        if recorded.status != DepositStatus::Unfinalised {
            return Err(DepositReject::ConflictingReceipt);
        }
        match recorded.uncredited {
            Some(uncredited) => {
                self.record_deposit(&recorded.receipt, DepositStatus::Reversed, Fixed::ZERO, Some(uncredited), events)?;
                self.track_uncredited(&recorded.receipt, uncredited, false);
                Ok(())
            }
            None => {
                self.move_deposit_cash(&recorded.receipt, false, true)?;
                self.record_deposit(
                    &recorded.receipt,
                    DepositStatus::Reversed,
                    -recorded.receipt.amount,
                    None,
                    events,
                )?;
                if let DepositBeneficiary::Trader { account } = recorded.receipt.beneficiary {
                    self.close_out_after_balance_change(account, events);
                }
                Ok(())
            }
        }
    }
}
