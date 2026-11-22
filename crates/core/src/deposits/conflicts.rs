//! One-live-receipt-per-deposit-id resolution for the exit's id-keyed refunds.

use crate::Fixed;
use crate::deposits::{DepositReceipt, DepositReject, DepositStatus};
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;

impl Engine {
    /// From `V88`, refuses `receipt`, in a block above the finalised frontier, at a vault deposit id
    /// below the frontier's deposit count. Every such id is a deposit at or below the frontier, whose
    /// receipt may still hold a pending or held leaf there, so this one would be a second receipt leaf
    /// at the id: the shape the exit refunds and credits both. The custody adapter's ordered reads
    /// never send one; the engine refuses it whatever sends it.
    /// Error cases:
    /// - from `V88`, the frontier's deposit count covers the receipt's id
    pub(super) fn check_id_above_frontier(&self, receipt: &DepositReceipt) -> Result<(), DepositReject> {
        let covered = self.custody_checkpoint.is_some_and(|checkpoint| {
            receipt.block > checkpoint.block && receipt.deposit_id < checkpoint.deposit_count
        });
        if self.protocol_version >= ProtocolVersion::V88 && covered {
            return Err(DepositReject::ConflictingReceipt);
        }
        Ok(())
    }

    /// Reverses every other live receipt above the finalised frontier at `receipt`'s deposit id, so
    /// the new canonical receipt holds the id alone. From `V38` the exit refunds pending deposits
    /// by id, which two live receipts at one id would alias. A settled (finalised) conflict refuses
    /// the new receipt: finality is final, and the custody adapter must resolve the collision. From
    /// `V83` any conflict refuses it: a receipt still canonical at another id moves there in the same
    /// reorganisation (`reorganise_deposits`), and one the chain dropped reverses by its own `Reverse`,
    /// so none is reversed for holding an id another claims.
    /// Error cases:
    /// - a conflicting receipt is finalised, or its credited principal can no longer be removed
    /// - from `V83`, any other live receipt holds the id
    pub(super) fn reverse_conflicting_receipts(
        &mut self,
        receipt: &DepositReceipt,
        exclude: Option<&str>,
        events: &mut Vec<Event>,
    ) -> Result<(), DepositReject> {
        if self.protocol_version < ProtocolVersion::V38
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
            if self.protocol_version >= ProtocolVersion::V83 {
                return Err(DepositReject::ConflictingReceipt);
            }
            self.reverse_live_receipt(&reference, events)?;
        }
        Ok(())
    }

    /// Activation-time resolution of duplicate live receipts surviving from before `V38`: at each
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
                self.close_out_after_reversal(&recorded.receipt, events);
                Ok(())
            }
        }
    }
}
