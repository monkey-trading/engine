//! Read-only quotes: the price a fill would get, from the same pricing and risk checks a fill uses, and
//! an order's preview, which adds what the fill leaves the account at.

use std::cell::RefCell;
use std::collections::BTreeMap;

use serde::Serialize;

use crate::Fixed;
use crate::engine::Engine;
use crate::event::Reject;
use crate::execution::{Exec, HouseTotals, PriceGuard, Priced};
use crate::spread_tier::SpreadTier;
use crate::types::{AccountId, MarketId, Side};
use crate::volume_tier::{AccountBands, VolumeTier};

/// What a market order of the quoted size would pay now, were it sequenced next with nothing moved.
#[derive(Clone, Debug, PartialEq)]
pub struct Quote {
    /// The average price per unit of the whole order, spread included.
    pub price: Fixed,
    /// The index mid the price is made from.
    pub mid: Fixed,
    /// The spread in the price, in whole basis points of the mid, truncated; `price` minus `mid` is
    /// exact.
    pub spread_bps: i64,
    /// Whether the order grows the account's absolute position in the market.
    pub increases: bool,
    /// Why a fill at this size would still be declined, once it passes pricing: not enough margin, or no
    /// house capacity, for the risk it adds; from `V84`, while a grid applies, not enough margin for the
    /// requirement an ordinary order that only reduces raises (#913, D7), which a close-all entry never
    /// meets. `None` when it would fill.
    pub declined: Option<Reject>,
}

/// What a market order of an exact size would do now, were it sequenced next with nothing moved: its fill,
/// what the fill leaves the account at, and why it would be refused.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OrderPreview {
    /// The fill, once the order prices; `None` when pricing refuses it, and `refused` says why.
    pub fill: Option<PreviewFill>,
    /// The account's equity after the fill, its market at the fill's index and every other at its mark;
    /// with no fill, at its marks now.
    pub equity: Fixed,
    /// The initial requirement after the fill (`MarginStanding::initial_margin`), valued as `equity` is.
    pub initial_margin: Fixed,
    /// The close-out level of the requirement its close-out reads after the fill, valued as `equity` is.
    pub maintenance_margin: Fixed,
    /// Why the order would be refused: by pricing, for margin or for house capacity; `None` when it would
    /// fill.
    pub refused: Option<Reject>,
}

/// The fill an order preview prices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PreviewFill {
    /// The quantity that fills: the order's, or for a reduce-only order at most the position's.
    pub qty: Fixed,
    /// The average price per unit, spread included.
    pub price: Fixed,
    /// The index mid the price is made from.
    pub mid: Fixed,
    /// The spread in the price, in whole basis points of the mid, truncated.
    pub spread_bps: i64,
    /// Whether the order grows the account's absolute position in the market.
    pub increases: bool,
}

/// Quotes over one state of the engine, which compute the house totals once however many orders
/// they price, and each account's volume tier once however many of its orders.
pub struct Quoter<'a> {
    engine: &'a Engine,
    house: HouseTotals,
    tiers: RefCell<BTreeMap<AccountId, Option<&'a VolumeTier>>>,
}

impl Engine {
    /// A [`Quoter`] over the engine as it is now.
    pub fn quoter(&self) -> Quoter<'_> {
        Quoter { engine: self, house: HouseTotals::of(self), tiers: RefCell::new(BTreeMap::new()) }
    }

    /// `Quoter::quote` over the engine as it is now, for a single order.
    pub fn quote(&self, account: AccountId, market: MarketId, side: Side, qty: Fixed) -> Result<Quote, Reject> {
        self.quoter().quote(account, market, side, qty)
    }

    /// `Quoter::preview` over the engine as it is now, for a single order.
    pub fn preview_order(
        &self,
        account: AccountId,
        market: MarketId,
        side: Side,
        qty: Fixed,
        reduce_only: bool,
    ) -> Option<OrderPreview> {
        self.quoter().preview(account, market, side, qty, reduce_only)
    }
}

impl Quoter<'_> {
    /// `tiers`, a market's spread bands, as `account` pays them: from `V59` with the first band ending
    /// at its tier's share of the first bound, read once per quoter, and as listed before it.
    pub fn account_bands<'b>(&self, account: AccountId, tiers: &'b [SpreadTier]) -> AccountBands<'b> {
        AccountBands::at(tiers, self.tier(account).map(|tier| tier.best_price_share_bps))
    }

    /// The tier of the table in force `account` trades at, read once per quoter.
    fn tier(&self, account: AccountId) -> Option<&VolumeTier> {
        *self.tiers.borrow_mut().entry(account).or_insert_with(|| self.engine.tier_in_force(account))
    }

    /// Prices a market order for `account` as `execute` would, without changing anything: the same
    /// `price` and, for an order that adds risk, the same margin and house exposure checks, and from `V84`
    /// for one that only reduces the check of the requirement it raises (`Engine::check_close`), with no
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
        let (exec, priced) = self.priced(account, market, side, qty, false)?;
        Ok(Quote {
            price: priced.price,
            mid: priced.mid,
            spread_bps: priced.spread_bps,
            increases: priced.increases,
            declined: self.declined(&exec, &priced),
        })
    }

    /// Previews a market order for `account` as `execute` would fill it, without changing anything: the
    /// same pricing and checks as `quote`, for a reduce-only order too, with what the fill leaves the
    /// account at. A refusal of the order's form or its pricing leaves no fill and the account as it is.
    /// `None` for an account never credited.
    pub fn preview(
        &self,
        account: AccountId,
        market: MarketId,
        side: Side,
        qty: Fixed,
        reduce_only: bool,
    ) -> Option<OrderPreview> {
        let engine = self.engine;
        engine.accounts.get(&account)?;
        let floor_bps = engine.margin_floor_of(self.tier(account));
        Some(match self.priced(account, market, side, qty, reduce_only) {
            Ok((exec, priced)) => {
                let after = engine.margin_after(&exec, &priced, floor_bps);
                OrderPreview {
                    fill: Some(PreviewFill {
                        qty: priced.qty,
                        price: priced.price,
                        mid: priced.mid,
                        spread_bps: priced.spread_bps,
                        increases: priced.increases,
                    }),
                    equity: after.equity,
                    initial_margin: after.initial_margin,
                    maintenance_margin: after.maintenance_margin,
                    refused: self.declined(&exec, &priced),
                }
            }
            Err(reason) => {
                let standing = engine.margin_standing(account).expect("credited");
                let (equity, _) = engine.equity_and_margin(&engine.accounts[&account], None);
                OrderPreview {
                    fill: None,
                    equity,
                    initial_margin: standing.initial_margin,
                    maintenance_margin: standing.maintenance_margin,
                    refused: Some(reason),
                }
            }
        })
    }

    /// The market order `quote` and `preview` price for `account`, and its price at the account's tier.
    /// Error cases:
    /// - as `quote`
    /// - a reduce-only order with no opposite position
    fn priced(
        &self,
        account: AccountId,
        market: MarketId,
        side: Side,
        qty: Fixed,
        reduce_only: bool,
    ) -> Result<(Exec, Priced), Reject> {
        let engine = self.engine;
        engine.orderable(account, market, qty, None)?;
        let exec = Exec {
            account,
            order_id: 0,
            market,
            side,
            qty,
            reduce_only,
            guard: PriceGuard::None,
            liquidation: false,
            close_checked: true,
        };
        let priced = engine.price(&exec, self.tier(account).map(|tier| tier.best_price_share_bps))?;
        Ok((exec, priced))
    }

    /// Why `exec`'s fill at `priced` would be declined once it has priced: the risk check of a fill that
    /// adds risk, and from `V84` the check of one that only reduces (`Engine::check_close`).
    fn declined(&self, exec: &Exec, priced: &Priced) -> Option<Reject> {
        let (engine, floor_bps) = (self.engine, self.engine.margin_floor_of(self.tier(exec.account)));
        if priced.increases {
            engine.check_risk(exec, priced, &self.house, floor_bps).err()
        } else {
            engine.check_close(exec, priced, floor_bps).err()
        }
    }
}
