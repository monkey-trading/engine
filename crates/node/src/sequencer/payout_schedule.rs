//! Bounded FIFO scheduling for unpaid approvals: first issuance, signer epochs and holder requests.
//! An authorization that expires unclaimed is never reissued by the clock.

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Unbounded};

use engine_core::types::AccountId;

use crate::checkpoints::APPROVAL_PAGE_SIZE;
use crate::sequencer::payouts::Payouts;

type Identity = (AccountId, u64);
type Order = (u64, usize);

/// Retains complete approval order until the sequencer acknowledges a finalized claim.
#[derive(Default)]
pub(super) struct Schedule {
    ordered: BTreeMap<Order, Identity>,
    pending: BTreeMap<Order, Identity>,
    epoch: Option<u64>,
    scan: Option<EpochScan>,
    /// The order of the oldest recovered approval not yet loaded, which may need a signature.
    unloaded: Option<Order>,
}

struct EpochScan {
    after: Option<Order>,
}

impl Schedule {
    /// Adds a newly approved or recovered identity to immutable FIFO order, due for signing or not.
    pub(super) fn insert(&mut self, order: Order, identity: Identity, due: bool) {
        assert!(self.ordered.insert(order, identity).is_none(), "approval orders are unique");
        if due {
            self.pending.insert(order, identity);
        }
    }

    /// The signer epoch of the latest custody evidence, once there is any.
    pub(super) fn epoch(&self) -> Option<u64> {
        self.epoch
    }

    /// Retires every delivery index only after finalized claim evidence is consumed.
    pub(super) fn retire(&mut self, order: Order) {
        self.ordered.remove(&order);
        self.pending.remove(&order);
    }

    /// Queues a holder's request for a fresh authorization; a repeat before it is signed is a no-op.
    pub(super) fn request(&mut self, order: Order) {
        if let Some(&identity) = self.ordered.get(&order) {
            self.pending.insert(order, identity);
        }
    }

    /// Records the oldest recovered approval not yet loaded; nothing after it is signed until it is.
    pub(super) fn hold_after(&mut self, unloaded: Option<Order>) {
        self.unloaded = unloaded;
    }

    /// Whether the approval already waits for an authorization.
    pub(super) fn is_pending(&self, order: Order) -> bool {
        self.pending.contains_key(&order)
    }

    /// The seq of the earliest approval due for an authorization that was approved after `after`, or of
    /// the earliest of all with none.
    pub(super) fn first_due_after(&self, after: Option<u64>) -> Option<u64> {
        let from = after.map_or(0, |after| after.saturating_add(1));
        self.pending.range((from, 0)..).next().map(|(&(seq, _), _)| seq)
    }

    /// Classifies a bounded prefix of an epoch transition.
    pub(super) fn prepare(&mut self, epoch: u64, payouts: &Payouts) {
        if self.epoch != Some(epoch) {
            self.epoch = Some(epoch);
            self.scan = Some(EpochScan { after: None });
        }
        if let Some(scan) = &mut self.scan {
            for (&order, &identity) in
                self.ordered.range((scan.after.map_or(Unbounded, Excluded), Unbounded)).take(APPROVAL_PAGE_SIZE)
            {
                let payout = payouts.get(&identity).expect("scheduled identities have unpaid approvals");
                if payout.released.is_none_or(|(released_epoch, _)| released_epoch != epoch) {
                    self.pending.insert(order, identity);
                }
                scan.after = Some(order);
            }
            if self.ordered.range((scan.after.map_or(Unbounded, Excluded), Unbounded)).next().is_none() {
                self.scan = None;
            }
        }
    }

    /// Inspects the earliest due approval after all earlier epoch entries were classified and all
    /// earlier recovered approvals were loaded.
    pub(super) fn next(&self) -> Option<(Order, Identity)> {
        let (&order, &identity) = self.pending.first_key_value()?;
        if self.unloaded.is_some_and(|unloaded| order > unloaded) {
            return None;
        }
        if let Some(scan) = &self.scan
            && scan.after.is_none_or(|after| order > after)
        {
            return None;
        }
        Some((order, identity))
    }

    /// Reserves one selected identity for this issuance batch; failures stop the sequencer.
    pub(super) fn selected(&mut self, order: Order) {
        self.pending.remove(&order);
    }

    /// Leaves a due approval unsigned until its holder asks again.
    pub(super) fn dismiss(&mut self, order: Order) {
        self.pending.remove(&order);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The earliest approval due for a signature past a seq: one signed or dismissed is not due, one its
    /// holder asks for again is, and one at the seq itself is not past it.
    #[test]
    fn the_first_due_after_a_seq_is_the_earliest_approval_past_it_that_waits_for_a_signature() {
        let identity = |byte: u8| (AccountId([byte; 20]), 1);
        let mut schedule = Schedule::default();
        schedule.insert((10, 0), identity(1), true);
        schedule.insert((20, 0), identity(2), false);
        schedule.insert((20, 1), identity(3), true);
        schedule.insert((30, 0), identity(4), true);
        assert_eq!(schedule.first_due_after(None), Some(10));
        assert_eq!(schedule.first_due_after(Some(10)), Some(20), "an approval at the seq itself");
        assert_eq!(schedule.first_due_after(Some(20)), Some(30));
        assert_eq!(schedule.first_due_after(Some(30)), None);
        schedule.selected((10, 0));
        schedule.dismiss((20, 1));
        assert_eq!(schedule.first_due_after(None), Some(30), "a signed or dismissed approval waits for nothing");
        schedule.request((20, 0));
        assert_eq!(schedule.first_due_after(Some(19)), Some(20), "its holder asked for a signature");
    }
}
