//! Bounded FIFO scheduling for unpaid approvals, expiries and signer epochs.

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
    expirations: BTreeMap<(u64, Order), Identity>,
    epoch: Option<u64>,
    scan: Option<EpochScan>,
}

struct EpochScan {
    after: Option<Order>,
}

impl Schedule {
    /// Adds a newly approved or recovered identity to immutable FIFO order.
    pub(super) fn insert(&mut self, order: Order, identity: Identity) {
        assert!(self.ordered.insert(order, identity).is_none(), "approval orders are unique");
        self.pending.insert(order, identity);
    }

    /// Retires every delivery index only after finalized claim evidence is consumed.
    pub(super) fn retire(&mut self, order: Order, released: Option<(u64, u64)>) {
        self.ordered.remove(&order);
        self.pending.remove(&order);
        if let Some((_, expires)) = released {
            self.expirations.remove(&(expires, order));
        }
    }

    /// Classifies a bounded expiry batch and bounded prefix of an epoch transition.
    /// Returns false until every currently expired authorization has entered FIFO order.
    pub(super) fn prepare(&mut self, epoch: u64, now: u64, payouts: &Payouts) -> bool {
        if self.epoch != Some(epoch) {
            self.epoch = Some(epoch);
            self.scan = Some(EpochScan { after: None });
        }
        for _ in 0..APPROVAL_PAGE_SIZE {
            let Some((&(expires, order), &identity)) = self.expirations.first_key_value() else { break };
            if expires > now {
                break;
            }
            self.expirations.pop_first();
            self.pending.insert(order, identity);
        }
        if self.expirations.first_key_value().is_some_and(|((expires, _), _)| *expires <= now) {
            return false;
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
        true
    }

    /// Inspects the earliest due approval after all earlier epoch entries were classified.
    pub(super) fn next(&self) -> Option<(Order, Identity)> {
        let (&order, &identity) = self.pending.first_key_value()?;
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

    /// Records delivery progress after its authorization entry is durable.
    pub(super) fn issued(&mut self, order: Order, identity: Identity, previous: Option<(u64, u64)>, expires: u64) {
        if let Some((_, expires)) = previous {
            self.expirations.remove(&(expires, order));
        }
        self.expirations.insert((expires, order), identity);
    }
}
