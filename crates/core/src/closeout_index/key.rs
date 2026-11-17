//! An account's key: a price range per position, and an FX range for a market quoted outside USD,
//! within which the account provably stays at or above its close-out level, and meets the weekend
//! requirement when its key says so.
//!
//! The bound is the check's own valuation at the ranges' corners. A position's PnL in USD and its
//! notional are each monotone in its price and in its FX index with the other held, every rounding in
//! them included, so over a box each takes its least and greatest values at a corner. Equity is the
//! balance plus a sum over positions, and the requirement a sum of bands that rise with notional, so
//! the balance plus each position's least PnL, against the level of the bands at each greatest
//! notional, bounds the check at every point inside. The ranges themselves are a guess that shares
//! the headroom between positions by their requirement; only the corners make a key sound.

use crate::Fixed;
use crate::account::{Account, Position};
use crate::closeout_index::markets::MarketRules;
use crate::closeout_index::valuation::{LIMIT, Valuations, abs, add, at_level, margin_of, position_value, sub};
use crate::engine::Engine;
use crate::fixed::Rounding;
use crate::market::MarketSpec;
use crate::types::MarketId;

/// The least a range may reach on its loss side, as a share of the price or FX index: one bp.
const MIN_REACH: Fixed = Fixed::from_bps(1);
/// How many times the ranges are halved before the account is left unkeyed.
const HALVINGS: u32 = 3;
/// How far a long's range reaches above its price for each unit it reaches below.
const LONG_GAIN_REACH: i64 = 2;
/// The most a long's range reaches above its price, and a short's, as shares of the price.
const LONG_RISE: Fixed = Fixed::ONE;
const SHORT_RISE: Fixed = Fixed::from_int(100);
/// The most an FX range reaches either way, as a share of the index, so a yen index stays positive.
const FX_REACH: Fixed = Fixed::from_raw(500_000_000);

/// Prices or FX indexes from `low` to `high`, both included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Range {
    pub(super) low: Fixed,
    pub(super) high: Fixed,
}

/// A position as its key holds it.
#[derive(Clone, Debug)]
pub(super) struct KeyedPosition {
    pub(super) market: MarketId,
    pub(super) position: Position,
    pub(super) price: Range,
    /// For a market quoted outside USD.
    pub(super) fx: Option<Range>,
    /// The greatest USD notional over the ranges' corners.
    max_notional: Fixed,
}

/// An account's key.
#[derive(Clone, Debug)]
pub(super) struct Key {
    /// The balance and positions it was computed for.
    pub(super) balance: Fixed,
    pub(super) positions: Vec<KeyedPosition>,
    /// The sum of each position's least PnL over its corners, and of the larger magnitude of its least
    /// and greatest, which bounds every running sum of the check's equity.
    worst_pnl: Fixed,
    pnl_span: Fixed,
    /// The sum of each position's requirement at its greatest notional: at its market's bands, and
    /// with each band at least its weekend margin.
    margin: Fixed,
    weekend_margin: Fixed,
    /// `margin` at the close-out level, and the balance plus `worst_pnl` less it, at least zero.
    level_margin: Fixed,
    pub(super) headroom: Fixed,
}

/// A position at the valuation a key is built around.
struct Point<'a> {
    market: MarketId,
    position: Position,
    spec: &'a MarketSpec,
    price: Fixed,
    fx: Option<Fixed>,
    pnl: Fixed,
    notional: Fixed,
    margin: Fixed,
}

#[cfg(any(test, feature = "closeout-oracle"))]
impl Range {
    /// Whether `at` lies in the range.
    pub(super) fn contains(&self, at: Fixed) -> bool {
        self.low <= at && at <= self.high
    }
}

#[cfg(any(test, feature = "closeout-oracle"))]
impl KeyedPosition {
    /// Whether `valuation` lies inside the position's ranges.
    pub(super) fn contains(&self, valuation: crate::closeout_index::valuation::Valuation) -> bool {
        let fx = self.fx.is_none_or(|range| valuation.fx.is_some_and(|fx| range.contains(fx)));
        fx && valuation.price.is_some_and(|price| self.price.contains(price))
    }
}

impl Key {
    /// Whether `acct` holds exactly the positions the key was computed for.
    pub(super) fn holds(&self, acct: &Account) -> bool {
        acct.positions.len() == self.positions.len()
            && acct
                .positions
                .iter()
                .zip(&self.positions)
                .all(|((id, pos), keyed)| *id == keyed.market && *pos == keyed.position)
    }

    /// Moves the key to `balance`, `Some` while it still shows the account at or above its level.
    pub(super) fn rebalance(&mut self, balance: Fixed, level_bps: i64, charge: Rounding) -> Option<()> {
        self.balance = balance;
        self.settle(level_bps, charge)
    }

    /// Recomputes the headroom at `level_bps`, `Some` while it is at least zero.
    pub(super) fn settle(&mut self, level_bps: i64, charge: Rounding) -> Option<()> {
        if self.weekend_margin > LIMIT || add(abs(self.balance)?, self.pnl_span)? > LIMIT {
            return None;
        }
        self.level_margin = at_level(self.margin, level_bps, charge)?;
        self.headroom = sub(add(self.balance, self.worst_pnl)?, self.level_margin)?;
        (!self.headroom.is_negative()).then_some(())
    }

    /// Moves the requirement of the position in `market` from bands `old` to `new`, `Some` while the
    /// key still shows the account at or above its level.
    pub(super) fn reband(
        &mut self,
        market: MarketId,
        (old, new): (&MarketRules, &MarketRules),
        level_bps: i64,
        charge: Rounding,
    ) -> Option<()> {
        let held = self.positions.iter().find(|keyed| keyed.market == market)?.max_notional;
        let (old_margin, old_weekend) = requirements(old, held, charge)?;
        let (new_margin, new_weekend) = requirements(new, held, charge)?;
        self.margin = add(sub(self.margin, old_margin)?, new_margin)?;
        self.weekend_margin = add(sub(self.weekend_margin, old_weekend)?, new_weekend)?;
        self.settle(level_bps, charge)
    }

    /// Whether the key does not show the account meeting its requirement with every weekend margin.
    pub(super) fn weekend_weak(&self) -> bool {
        add(self.balance, self.worst_pnl).is_none_or(|equity| equity < self.weekend_margin)
    }
}

impl Engine {
    /// A key for `acct` at `valuations`, or `None` when it is below its level, a held market has no
    /// point valuation, its ranges would be narrower than `MIN_REACH`, or a step overflows.
    pub(super) fn key_account(&self, acct: &Account, valuations: &Valuations, level_bps: i64) -> Option<Key> {
        let charge = self.charge_rounding();
        let mut points = Vec::with_capacity(acct.positions.len());
        let (mut equity, mut margin) = (acct.balance, Fixed::ZERO);
        for (id, pos) in &acct.positions {
            let spec = &self.markets.get(id)?.spec;
            let valuation = valuations.get(id).filter(|valuation| valuation.is_point(spec.quote_currency))?;
            let price = valuation.price?;
            let (pnl, notional) =
                position_value(self.protocol_version, charge, spec.quote_currency, pos, price, valuation.fx)?;
            let required = margin_of(&spec.margin_tiers, notional, 0, charge)?;
            (equity, margin) = (add(equity, pnl)?, add(margin, required)?);
            let point =
                Point { market: *id, position: *pos, spec, price, fx: valuation.fx, pnl, notional, margin: required };
            points.push(point);
        }
        if margin.is_zero() {
            return None;
        }
        let headroom = sub(equity, at_level(margin, level_bps, charge)?)?;
        (0..=HALVINGS).find_map(|halving| {
            let ranges = points
                .iter()
                .map(|point| {
                    let share = headroom.checked_mul_div(point.margin, margin, Rounding::Down)?;
                    point.ranges(Fixed::from_raw(share.raw() >> halving), level_bps)
                })
                .collect::<Option<Vec<_>>>()?;
            self.bound(acct.balance, &points, ranges, level_bps)
        })
    }

    /// The key over `ranges`, one per point, from the check's valuation at their corners, or `None` when
    /// it does not show the account at or above its level or a step overflows.
    fn bound(
        &self,
        balance: Fixed,
        points: &[Point],
        ranges: Vec<(Range, Option<Range>)>,
        level_bps: i64,
    ) -> Option<Key> {
        let (version, charge) = (self.protocol_version, self.charge_rounding());
        let mut key = Key {
            balance,
            positions: Vec::with_capacity(points.len()),
            worst_pnl: Fixed::ZERO,
            pnl_span: Fixed::ZERO,
            margin: Fixed::ZERO,
            weekend_margin: Fixed::ZERO,
            level_margin: Fixed::ZERO,
            headroom: Fixed::ZERO,
        };
        for (point, (price, fx)) in points.iter().zip(ranges) {
            let fxs = fx.map_or([point.fx; 2], |range| [Some(range.low), Some(range.high)]);
            let mut corners = [price.low, price.high].into_iter().flat_map(|at| fxs.map(|fx| (at, fx)));
            let (worst, best, max_notional) =
                corners.try_fold((None::<Fixed>, None::<Fixed>, Fixed::ZERO), |(worst, best, most), (at, fx)| {
                    let (pnl, notional) =
                        position_value(version, charge, point.spec.quote_currency, &point.position, at, fx)?;
                    Some((
                        Some(worst.map_or(pnl, |w| w.min(pnl))),
                        Some(best.map_or(pnl, |b| b.max(pnl))),
                        most.max(notional),
                    ))
                })?;
            let (worst, best) = (worst?, best?);
            let rules = MarketRules::of(point.spec);
            let (required, weekend) = requirements(&rules, max_notional, charge)?;
            key.worst_pnl = add(key.worst_pnl, worst)?;
            key.pnl_span = add(key.pnl_span, abs(worst)?.max(abs(best)?))?;
            key.margin = add(key.margin, required)?;
            key.weekend_margin = add(key.weekend_margin, weekend)?;
            key.positions.push(KeyedPosition {
                market: point.market,
                position: point.position,
                price,
                fx,
                max_notional,
            });
        }
        key.settle(level_bps, charge).map(|()| key)
    }
}

impl Point<'_> {
    /// The ranges for this position's `share` of the headroom: a long reaches down by what the share
    /// covers of its PnL and, `LONG_GAIN_REACH` times as far up, of its requirement at the level; a
    /// short reaches up by what it covers of both and down to zero, which costs it nothing; an FX
    /// index takes a quarter of the share either way. `None` when the loss side would reach less than
    /// `MIN_REACH` or a step overflows.
    fn ranges(&self, share: Fixed, level_bps: i64) -> Option<(Range, Option<Range>)> {
        if !self.notional.is_positive() {
            return None;
        }
        let level_rate =
            at_level(self.margin, level_bps, Rounding::Up)?.checked_mul_div(Fixed::ONE, self.notional, Rounding::Up)?;
        let price_share = if self.fx.is_some() {
            share.checked_mul_div(Fixed::from_int(3), Fixed::from_int(4), Rounding::Down)?
        } else {
            share
        };
        let per_unit = |reach: i64| {
            let rate = add(Fixed::ONE, Fixed::from_raw(level_rate.raw().checked_mul(i128::from(reach))?))?;
            self.notional.checked_mul_div(rate, Fixed::ONE, Rounding::Up)
        };
        let price = if self.position.qty.is_positive() {
            let fall = reach(price_share, per_unit(LONG_GAIN_REACH)?, Fixed::ONE)?;
            let rise = Fixed::from_raw(fall.raw().checked_mul(i128::from(LONG_GAIN_REACH))?).min(LONG_RISE);
            let low = if fall >= Fixed::ONE {
                Fixed::ZERO
            } else {
                sub(self.price, scaled(self.price, fall, Rounding::Up)?)?
            };
            Range { low, high: add(self.price, scaled(self.price, rise, Rounding::Down)?)? }
        } else {
            let rise = reach(price_share, per_unit(1)?, SHORT_RISE)?;
            Range { low: Fixed::ZERO, high: add(self.price, scaled(self.price, rise, Rounding::Down)?)? }
        };
        let fx = match self.fx {
            None => None,
            Some(fx) => {
                let quarter = share.checked_mul_div(Fixed::ONE, Fixed::from_int(4), Rounding::Down)?;
                let reach = reach(quarter, add(per_unit(1)?, abs(self.pnl)?)?, FX_REACH)?;
                let (low, high) =
                    (sub(fx, scaled(fx, reach, Rounding::Up)?)?, add(fx, scaled(fx, reach, Rounding::Down)?)?);
                Some(Range { low, high })
            }
        };
        Some((price, fx))
    }
}

/// `cover / cost` as a share, at most `cap`, or `None` when it is under `MIN_REACH`.
fn reach(cover: Fixed, cost: Fixed, cap: Fixed) -> Option<Fixed> {
    if !cost.is_positive() {
        return None;
    }
    let share = cover.checked_mul_div(Fixed::ONE, cost, Rounding::Down).map_or(cap, |share| share.min(cap));
    (share >= MIN_REACH).then_some(share)
}

/// `value` times `share`.
fn scaled(value: Fixed, share: Fixed, rounding: Rounding) -> Option<Fixed> {
    value.checked_mul_div(share, Fixed::ONE, rounding)
}

/// The requirement at `rules`' bands for a position of `notional`, and with each band at least the
/// market's weekend margin.
fn requirements(rules: &MarketRules, notional: Fixed, charge: Rounding) -> Option<(Fixed, Fixed)> {
    let required = margin_of(&rules.margin_tiers, notional, 0, charge)?;
    let weekend = margin_of(&rules.margin_tiers, notional, rules.weekend_margin_bps.max(0), charge)?;
    Some((required, weekend))
}

#[cfg(feature = "closeout-oracle")]
impl Engine {
    /// The headroom `key`'s ranges give at the bands in force and `level_bps`, computed afresh as keying
    /// computes it, or `None` below zero.
    pub(super) fn rebound(&self, key: &Key, level_bps: i64) -> Option<Fixed> {
        let points = key
            .positions
            .iter()
            .map(|keyed| {
                let spec = &self.markets.get(&keyed.market)?.spec;
                let (price, fx, zero) = (keyed.price.low, None, Fixed::ZERO);
                Some(Point {
                    market: keyed.market,
                    position: keyed.position,
                    spec,
                    price,
                    fx,
                    pnl: zero,
                    notional: zero,
                    margin: zero,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let ranges = key.positions.iter().map(|keyed| (keyed.price, keyed.fx)).collect();
        self.bound(key.balance, &points, ranges, level_bps).map(|fresh| fresh.headroom)
    }
}
