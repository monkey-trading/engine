//! Read-only quotes: the price a fill would get, from the same pricing and risk checks a fill uses.

use crate::Fixed;
use crate::engine::Engine;
use crate::event::Reject;
use crate::execution::{Exec, HouseTotals, PriceGuard};
use crate::types::{AccountId, MarketId, Side};

/// What a market order of the quoted size would pay now, were it sequenced next with nothing moved.
#[derive(Clone, Debug, PartialEq)]
pub struct Quote {
    /// The average price per unit of the whole order, spread included.
    pub price: Fixed,
    /// The index mid the price is made from.
    pub mid: Fixed,
    /// The spread in the price, in basis points of the mid.
    pub spread_bps: i64,
    /// Whether the order grows the account's absolute position in the market.
    pub increases: bool,
    /// Why a fill at this size would still be declined, once it passes pricing: not enough margin, or no
    /// house capacity. `None` when it would fill.
    pub declined: Option<Reject>,
}

/// Quotes over one state of the engine, which compute the house totals once however many orders
/// they price.
pub struct Quoter<'a> {
    engine: &'a Engine,
    house: HouseTotals,
}

impl Engine {
    /// A [`Quoter`] over the engine as it is now.
    pub fn quoter(&self) -> Quoter<'_> {
        Quoter { engine: self, house: HouseTotals::of(self) }
    }

    /// `Quoter::quote` over the engine as it is now, for a single order.
    pub fn quote(&self, account: AccountId, market: MarketId, side: Side, qty: Fixed) -> Result<Quote, Reject> {
        self.quoter().quote(account, market, side, qty)
    }
}

impl Quoter<'_> {
    /// Prices a market order for `account` as `execute` would, without changing anything: the same
    /// `price` and, for an order that adds risk, the same margin and house exposure checks, with no
    /// slippage guard. It reads the market's stored open flag and stress level, which every sequenced
    /// command leaves current, so a fill after more time has passed can only pay the same or less
    /// stress, and no more than this price unless the index, house exposure, or the account moved.
    /// Access screening is not part of it. A fill's cost rounds up to a unit before it is divided by
    /// the order's size, so within a spread tier a smaller order can price up to a unit of cost per
    /// unit of size above a larger one; at an order of a cent's notional that is about a thousandth of
    /// a basis point, and a slippage bound of a basis point covers it.
    /// Error cases:
    /// - protocol parameters are not set yet, or the market is unknown
    /// - `qty` at or below zero or above `Fixed::MAX_INPUT`
    /// - the account has never been credited
    /// - the market is closed, or has no valid index, or its FX market has no price
    /// - new risk in a reduce-only market, or below the market's minimum order notional
    /// - a spread at this size that reaches the price itself
    pub fn quote(&self, account: AccountId, market: MarketId, side: Side, qty: Fixed) -> Result<Quote, Reject> {
        let engine = self.engine;
        engine.orderable(account, market, qty, None)?;
        let exec = Exec {
            account,
            order_id: 0,
            market,
            side,
            qty,
            reduce_only: false,
            guard: PriceGuard::None,
            liquidation: false,
        };
        let priced = engine.price(&exec)?;
        let declined = if priced.increases { engine.check_risk(&exec, &priced, &self.house).err() } else { None };
        Ok(Quote {
            price: priced.price,
            mid: priced.mid,
            spread_bps: priced.spread_bps,
            increases: priced.increases,
            declined,
        })
    }
}
