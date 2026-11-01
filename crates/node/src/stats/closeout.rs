//! The estimated close-out price of one position: where the market must move for its account to
//! fall below the close-out level, with the account's other positions held at their marks.

use engine_core::Fixed;
use engine_core::fixed::Rounding;
use engine_core::market::MarketSpec;

/// Where a position's account closes out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reach {
    /// The account is below the close-out level at the marks already.
    Due,
    /// A move of this many basis points of the mark, negative for a fall.
    At(i64),
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
            return Reach::At(crossing.bps_of(notional) - 10_000);
        }
        near = (far, far_slack);
    }
    unreachable!("the slack is negative at the edge, so the walk crosses zero by it")
}
