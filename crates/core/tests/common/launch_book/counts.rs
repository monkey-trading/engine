//! What a built book is checked against: the cash a weak account holds, what the engine says it holds,
//! and that it reads back what was written.

use std::collections::BTreeMap;

use engine_core::types::MarketId;
use engine_core::{Engine, Fixed};

use crate::common::Harness;
use crate::common::launch_book::mix::{Mix, WeakKind};

/// The cash a weak account holds (`WeakKind`).
pub(super) fn weak_cash(
    engine: &Engine,
    account: &engine_core::account::Account,
    market: MarketId,
    kind: &WeakKind,
) -> Fixed {
    let mut bare = account.clone();
    bare.balance = Fixed::ZERO;
    match kind {
        WeakKind::FallsBelowAt(price) => {
            let level = engine.params().expect("params").closeout_level_bps;
            let (equity, margin) = engine.equity_and_margin(&bare, Some((market, *price, None)));
            margin.mul_bps(level) - equity - Fixed::ONE
        }
        WeakKind::BelowWeekendMargin => engine.equity_and_margin(&bare, None).1,
    }
}

/// What the book holds, counted from the engine over every account, the named ones included.
pub(super) struct Counts {
    pub holders: BTreeMap<MarketId, usize>,
    pub positions: usize,
    pub orders: usize,
    pub tiers: Vec<usize>,
    pub resting_accounts: usize,
}

impl Counts {
    pub fn of(engine: &Engine, tiers: usize) -> Self {
        let mut counts =
            Counts { holders: BTreeMap::new(), positions: 0, orders: 0, tiers: vec![0; tiers], resting_accounts: 0 };
        for (id, account) in engine.accounts() {
            for market in account.positions.keys() {
                *counts.holders.entry(*market).or_default() += 1;
            }
            counts.positions += account.positions.len();
            counts.orders += account.orders.len();
            counts.resting_accounts += usize::from(!account.orders.is_empty());
            counts.tiers[engine.volume_tier(*id)] += 1;
        }
        counts
    }
}

/// What the engine reads back is what was written: the accounts, and each market's aggregates and
/// resting orders from the accounts'.
pub(super) fn check(h: &Harness, mix: &Mix) {
    let engine = &h.engine;
    assert_eq!(engine.accounts().len(), mix.accounts);
    // Every account that holds a position has traded, so has a volume book.
    for (id, account) in engine.accounts() {
        assert!(account.positions.is_empty() || engine.account_volume(*id).is_some(), "{id:?} has no volume book");
    }
    for (id, market) in engine.markets() {
        let positions = engine.accounts().values().filter_map(|account| account.positions.get(id));
        let (qty, cost) = positions.fold((Fixed::ZERO, Fixed::ZERO), |(q, c), p| (q + p.qty, c + p.cost()));
        assert_eq!((market.client_qty, market.client_cost), (qty, cost), "market {id}'s aggregates");
        let resting = engine.accounts().values().flat_map(|a| a.orders.values()).filter(|o| o.market == *id).count();
        assert_eq!(market.resting.len(), resting, "market {id}'s resting orders");
    }
}
