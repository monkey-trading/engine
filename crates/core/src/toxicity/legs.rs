//! From `V54`, the markouts a new fill may match, indexed by account, market, side and leg, so a fill
//! takes from the newest without scanning the queue. The index is derived from the queue and kept
//! with the hot indexes, so it is not state: a markout joins it as it is queued with a quantity left
//! to match, leaves it once matched in full or scored, and the whole index is rebuilt from the queue
//! after recovery or a split.

use std::collections::{BTreeMap, VecDeque};

use crate::Fixed;
use crate::engine::{Engine, Markout};
use crate::types::{AccountId, MarketId, Side};

/// Which of a markout's quantities left to match a new fill takes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Leg {
    /// What its fill opened, which a later fill's close matches.
    Open,
    /// What its fill closed, which a later fill's open matches.
    Closed,
}

impl Leg {
    fn of(self, markout: &mut Markout) -> &mut Fixed {
        match self {
            Leg::Open => &mut markout.open,
            Leg::Closed => &mut markout.closed,
        }
    }
}

/// The queue indices of the markouts with a quantity of each leg left to match, oldest first.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct MarkoutLegs(BTreeMap<(AccountId, MarketId, Side, Leg), VecDeque<u64>>);

impl MarkoutLegs {
    /// The index of `markouts`, whose front is at queue index `base`.
    pub(crate) fn of_queue(markouts: &VecDeque<Markout>, base: u64) -> Self {
        let mut legs = Self::default();
        for (position, markout) in markouts.iter().enumerate() {
            legs.add(markout, base + position as u64);
        }
        legs
    }

    /// Indexes the markout at queue index `at`, the newest indexed.
    fn add(&mut self, markout: &Markout, at: u64) {
        for (leg, left) in [(Leg::Open, markout.open), (Leg::Closed, markout.closed)] {
            if left.is_positive() {
                self.0.entry((markout.account, markout.market, markout.side, leg)).or_default().push_back(at);
            }
        }
    }

    /// Forgets the markout at queue index `at` as it leaves the queue, the oldest it can hold.
    fn retire(&mut self, markout: &Markout, at: u64) {
        for leg in [Leg::Open, Leg::Closed] {
            let key = (markout.account, markout.market, markout.side, leg);
            if let Some(queue) = self.0.get_mut(&key)
                && queue.front() == Some(&at)
            {
                queue.pop_front();
                if queue.is_empty() {
                    self.0.remove(&key);
                }
            }
        }
    }
}

impl Engine {
    /// Indexes the markout just queued, which `newest` says went in behind every other; one that went
    /// in ahead of others moved their queue indices, so the index is rebuilt.
    pub(super) fn index_markout(&mut self, newest: bool) {
        let base = self.markout_base;
        let Some(indexes) = self.hot_indexes.as_mut() else { return };
        match self.markouts.back().filter(|_| newest) {
            Some(markout) => indexes.markout_legs.add(markout, base + self.markouts.len() as u64 - 1),
            None => indexes.markout_legs = MarkoutLegs::of_queue(&self.markouts, base),
        }
    }

    /// Forgets the markout `pop_markout` just took from the front of the queue.
    pub(super) fn retire_markout(&mut self, markout: &Markout) {
        // From `V9`, which `V54` follows, the pop moved the front's index up past it.
        let at = self.markout_base - 1;
        if let Some(indexes) = self.hot_indexes.as_mut() {
            indexes.markout_legs.retire(markout, at);
        }
    }

    /// Takes `qty` of a new fill on `side` from the `leg` quantity of `account`'s markouts in
    /// `market` on the other side, newest first, and returns the quantity of it taken from markouts
    /// whose fill was at `index`, which each of them, and the new fill, count as matched. A close-out
    /// passes no index and matches nothing. Only markouts within the horizon, not yet due, match.
    pub(super) fn match_round_trips(
        &mut self,
        (account, market, side): (AccountId, MarketId, Side),
        qty: Fixed,
        index: Option<Fixed>,
        leg: Leg,
    ) -> Fixed {
        if qty.is_zero() {
            return Fixed::ZERO;
        }
        self.ensure_hot_indexes();
        let key = (account, market, side.opposite(), leg);
        let (now, base) = (self.now, self.markout_base);
        let (mut left, mut matched) = (qty, Fixed::ZERO);
        while left.is_positive() {
            let legs = &mut self.hot_indexes.as_mut().expect("ensured above").markout_legs.0;
            let Some(queue) = legs.get_mut(&key) else { break };
            let position = queue.back().and_then(|at| usize::try_from(at.checked_sub(base)?).ok());
            let Some(markout) = position.and_then(|position| self.markouts.get_mut(position)).filter(|m| m.due > now)
            else {
                // The newest has fallen due, so every older one has too.
                legs.remove(&key);
                break;
            };
            let price = markout.price;
            let remaining = leg.of(markout);
            let taken = (*remaining).min(left);
            *remaining -= taken;
            let emptied = remaining.is_zero();
            left -= taken;
            if index == Some(price) {
                markout.matched += taken;
                matched += taken;
            }
            if emptied {
                queue.pop_back();
                if queue.is_empty() {
                    legs.remove(&key);
                }
            }
            if taken.is_positive() {
                self.record_markout_change(position.expect("found above"));
            }
        }
        matched
    }
}
