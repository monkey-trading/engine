//! The closed distribution epochs, held in a bounded ring: `GET /v1/monkey/distributions`.

use std::collections::VecDeque;

use engine_core::staking::UsdcUnits;
use engine_core::types::{AccountId, Millis};
use engine_core::{Engine, Fixed};
use serde::Serialize;

use crate::stats::Stats;

/// Most epochs per page.
pub(crate) const MAX_DISTRIBUTIONS_PAGE: usize = 200;

/// A recipient of a close's operator share and what it was allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct OperatorShare {
    pub account: AccountId,
    pub amount: UsdcUnits,
}

/// Epochs no command witnessed the end of, which the close that did distributed nothing for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Skipped {
    pub first: u64,
    pub last: u64,
}

/// One closed epoch, as its command's events said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EpochClose {
    pub epoch: u64,
    /// The seq of the command that closed it, and its sequenced time: the first to reach the epoch's
    /// end.
    pub close_seq: u64,
    pub close_ms: Millis,
    /// When the epoch ended.
    pub ended_ms: Millis,
    pub nav: Fixed,
    pub floor: Fixed,
    /// Whole USDC base units above the floor.
    pub excess_usdc_units: UsdcUnits,
    /// What the close allocated: to stakers and to the operator-share recipients.
    pub allocated_usdc_units: UsdcUnits,
    /// What went to stakers, and how many stakers it reached.
    pub stakers_usdc_units: UsdcUnits,
    pub stakers: u64,
    /// The recipients of the operator's share, in the order the close allocated them: none before
    /// `V47` or, before `V50`, for a close with rotation off; from `V50`, each account whose time in
    /// office in the epoch earned a part.
    pub operator_shares: Vec<OperatorShare>,
    /// The epochs before this one that no command witnessed the end of, when there are any.
    pub skipped: Option<Skipped>,
}

/// What `GET /v1/monkey/distributions` answers.
#[derive(Serialize)]
pub struct DistributionsPage {
    seq: Option<u64>,
    time_ms: Millis,
    /// Closed epochs, newest first.
    epochs: Vec<EpochClose>,
    /// The `before` for the page after this one; `None` when none is held.
    next: Option<u64>,
    /// Where the record behind the epochs begins: a close before it is not among them.
    begins_ms: Option<Millis>,
    begins_seq: Option<u64>,
    from_genesis: bool,
    /// Whether the history has dropped older epochs for room, and the oldest epoch it holds: a page
    /// that ends with `next` null is then the end of what is held, not of what happened.
    truncated: bool,
    retained_from_epoch: Option<u64>,
}

/// The newest `capacity` closed epochs, oldest first.
pub(crate) struct Distributions {
    capacity: usize,
    epochs: VecDeque<EpochClose>,
    /// Whether the ring has dropped an epoch for room.
    evicted: bool,
}

impl Distributions {
    pub(crate) fn new(capacity: usize) -> Self {
        Self { capacity, epochs: VecDeque::new(), evicted: false }
    }

    /// Adds the newest close, dropping the oldest past capacity.
    pub(crate) fn push(&mut self, close: EpochClose) {
        if self.epochs.len() == self.capacity {
            self.epochs.pop_front();
            self.evicted = true;
        }
        self.epochs.push_back(close);
    }

    /// Up to `limit` epochs closed before epoch `before`, newest first, and the epoch to pass as
    /// `before` for the page after them when more are held. Epochs are held in order, so this seeks.
    pub(crate) fn page(&self, before: Option<u64>, limit: usize) -> (Vec<EpochClose>, Option<u64>) {
        let end = before.map_or(self.epochs.len(), |before| self.epochs.partition_point(|close| close.epoch < before));
        let mut older = self.epochs.range(..end).rev();
        let page: Vec<EpochClose> = older.by_ref().take(limit).cloned().collect();
        let next = older.next().and(page.last()).map(|last| last.epoch);
        (page, next)
    }
}

impl Stats {
    /// The closed epochs before `before`, newest first.
    /// Error cases:
    /// - the limit is outside 1 to `MAX_DISTRIBUTIONS_PAGE`
    pub(crate) fn distributions(
        &self,
        engine: &Engine,
        before: Option<u64>,
        limit: usize,
    ) -> Result<DistributionsPage, String> {
        if !(1..=MAX_DISTRIBUTIONS_PAGE).contains(&limit) {
            return Err(format!("limit must be from 1 to {MAX_DISTRIBUTIONS_PAGE}"));
        }
        let inner = self.lock();
        let (epochs, next) = inner.distributions.page(before, limit);
        Ok(DistributionsPage {
            seq: engine.last_seq(),
            time_ms: engine.now(),
            epochs,
            next,
            begins_ms: inner.begin.map(|begin| begin.ts),
            begins_seq: inner.begin.map(|begin| begin.seq),
            from_genesis: inner.begins_at_genesis(),
            truncated: inner.distributions.evicted,
            retained_from_epoch: inner.distributions.epochs.front().map(|close| close.epoch),
        })
    }
}
