//! The estimated close-out price of one position: where the market must move for its account to
//! fall below the close-out level, with the account's other positions held at their marks.

use engine_core::account::{Account, Position as Held};
use engine_core::engine::MarketState;
use engine_core::fixed::Rounding;
use engine_core::market::MarketSpec;
use engine_core::types::MarketId;
use engine_core::{Engine, Fixed};
use serde::Serialize;

/// How far a long can fall, to nothing, before its close-out price counts as none.
const LONG_RANGE_BPS: u32 = 10_000;
/// How far a short can rise, to ten times its mark, before its close-out price counts as none.
const SHORT_RANGE_BPS: u32 = 90_000;

/// Whether an account's position is below its close-out level at the marks, or where it closes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReachKind {
    Due,
    At,
    Beyond,
}

/// One position's close-out, as the account read serves it. `price` is set only for `at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct PositionCloseout {
    pub market: MarketId,
    pub reach: ReachKind,
    pub price: Option<Fixed>,
}

/// Where a position's account closes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reach {
    /// The account is below the close-out level at the marks already.
    Due,
    /// The close-out price over the mark: below one for a long, above it for a short.
    At(Fixed),
    /// No move inside the range closes the account out.
    Beyond,
}

/// The account's standing at its marks and the close-out rule it is held to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Standing {
    pub equity: Fixed,
    pub margin: Fixed,
    /// The close-out level, in basis points of the margin requirement.
    pub level_bps: i64,
}

/// A position's notional and side, and the market's margin bands.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Position<'a> {
    pub notional: Fixed,
    pub long: bool,
    pub spec: &'a MarketSpec,
}

/// How far the market must move against `position`, within `range_bps` of its mark, before
/// `standing` falls below its close-out level. Only the position's own notional moves: its margin
/// follows the market's bands, its profit or loss moves one for one, and every other position
/// stays at its mark. The result is an estimate: it takes the account's equity and margin at the
/// marks as the engine reports them and rounds the margin bands up.
pub(crate) fn reach(standing: Standing, position: Position<'_>, range_bps: u32) -> Reach {
    let Position { notional, long, spec } = position;
    let margin_at = |at: Fixed| spec.margin_for(at, Rounding::Up);
    let held = margin_at(notional);
    // Equity less the level's share of the margin at a notional `at`, the position's alone moving.
    let slack = |at: Fixed| {
        let pnl = if long { at - notional } else { notional - at };
        let margin = standing.margin - held + margin_at(at);
        standing.equity + pnl - margin.mul_bps_rounded(standing.level_bps, Rounding::Up)
    };
    let at_mark = slack(notional);
    if at_mark.is_negative() {
        return Reach::Due;
    }
    let move_by = notional.mul_bps(i64::from(range_bps));
    let edge = if long { notional - move_by } else { notional + move_by };
    let at_edge = slack(edge);
    if !at_edge.is_negative() {
        return Reach::Beyond;
    }
    // The slack is linear inside a margin band, so the walk from the mark to the edge crosses zero
    // in one band, where it interpolates. The walk meets the bands' bounds in order, then the edge.
    let tiers = &spec.margin_tiers;
    let (low, high) = (edge.min(notional), edge.max(notional));
    let mut near = (notional, at_mark);
    let bounds = (0..tiers.len())
        .map(|step| &tiers[if long { tiers.len() - 1 - step } else { step }])
        .filter_map(|tier| tier.up_to)
        .filter(|bound| *bound > low && *bound < high);
    for far in bounds.chain(std::iter::once(edge)) {
        let far_slack = slack(far);
        if far_slack.is_negative() {
            let (from, from_slack) = near;
            let crossing = from + (far - from).mul_div(from_slack, from_slack - far_slack, Rounding::TowardZero);
            // Rounded up so a crossing too small for the ratio's nine places stays a positive price.
            return Reach::At(crossing.mul_div(Fixed::ONE, notional, Rounding::Up));
        }
        near = (far, far_slack);
    }
    unreachable!("the slack is negative at the edge, so the walk crosses zero by it")
}

/// As `reach`, for a position in the market that converts its own quote currency, USD-JPY: its USD
/// notional stays the quantity whatever the price, so its margin does not move, and its USD profit
/// is `quantity * entry * (1/mark - 1/price)` for a long, which falls faster than the price does.
/// `quantity` is the position's absolute size.
pub(crate) fn reach_self_converting(
    standing: Standing,
    long: bool,
    quantity: Fixed,
    entry: Fixed,
    mark: Fixed,
    range_bps: u32,
) -> Reach {
    let slack = standing.equity - standing.margin.mul_bps_rounded(standing.level_bps, Rounding::Up);
    if slack.is_negative() {
        return Reach::Due;
    }
    // The close-out price over the mark is `1 / (1 + t)` for a long and `1 / (1 - t)` for a short,
    // with `t = slack * mark / (quantity * entry)`.
    let Some(t) = slack.checked_mul_div(mark, quantity.mul(entry), Rounding::TowardZero) else { return Reach::Beyond };
    let divisor = if long { Fixed::ONE + t } else { Fixed::ONE - t };
    if !divisor.is_positive() {
        return Reach::Beyond;
    }
    let ratio = Fixed::ONE.div_rounded(divisor, Rounding::Up);
    // As `reach`, a crossing at the range's edge is none: the account is first closed out past it.
    if (ratio - Fixed::ONE).abs() >= Fixed::ONE.mul_bps(i64::from(range_bps)) {
        Reach::Beyond
    } else {
        Reach::At(ratio)
    }
}

/// A close-out price over the mark as a move in whole basis points, negative for a fall.
pub(crate) fn offset_bps(ratio: Fixed) -> i64 {
    ratio.bps_of(Fixed::ONE) - 10_000
}

/// The price a position is valued at, its market's mark or its entry before one, and its USD
/// notional there.
pub(crate) fn valued(market: &MarketState, position: &Held) -> (Fixed, Fixed) {
    let price = market.mark.unwrap_or(position.entry);
    (price, market.usd_at_mark(position.qty.abs().mul_rounded(price, Rounding::Up), Rounding::Up))
}

/// Where the account holding `position` in market `id` closes out, for an account of `equity` and
/// `margin` at the marks, the position valued at `(price, notional)` as `valued` gives: the one rule behind the position maps and an account's own read. A market
/// converting its own currency takes the walk that follows its price; none is due when the node has
/// no close-out level yet.
pub(crate) fn position_reach(
    engine: &Engine,
    (equity, margin): (Fixed, Fixed),
    id: MarketId,
    position: &Held,
    (price, notional): (Fixed, Fixed),
    range_bps: u32,
) -> Reach {
    let market = &engine.markets()[&id];
    let long = position.qty.is_positive();
    match engine.params().map(|params| params.closeout_level_bps).filter(|_| notional.is_positive()) {
        Some(level_bps) if market.spec.quote_currency.fx_market() == Some(id) => {
            let standing = Standing { equity, margin, level_bps };
            reach_self_converting(standing, long, position.qty.abs(), position.entry, price, range_bps)
        }
        Some(level_bps) => {
            let position = Position { notional, long, spec: &market.spec };
            reach(Standing { equity, margin, level_bps }, position, range_bps)
        }
        None => Reach::Beyond,
    }
}

/// The close-out of each of `account`'s positions, in market order, with the account's other
/// positions held at their marks. The price is in the market's quote currency.
pub(crate) fn account_closeouts(engine: &Engine, account: &Account, standing: (Fixed, Fixed)) -> Vec<PositionCloseout> {
    account
        .positions
        .iter()
        .map(|(id, position)| {
            let long = position.qty.is_positive();
            let range = if long { LONG_RANGE_BPS } else { SHORT_RANGE_BPS };
            let (mark, notional) = valued(&engine.markets()[id], position);
            let (reach, price) = match position_reach(engine, standing, *id, position, (mark, notional), range) {
                Reach::Due => (ReachKind::Due, None),
                Reach::Beyond => (ReachKind::Beyond, None),
                Reach::At(ratio) => {
                    let rounding = if long { Rounding::Up } else { Rounding::Down };
                    (ReachKind::At, Some(mark.mul_rounded(ratio, rounding)))
                }
            };
            PositionCloseout { market: *id, reach, price }
        })
        .collect()
}

#[cfg(test)]
mod tests;
