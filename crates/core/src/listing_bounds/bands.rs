//! The band bounds a listing notice and a voted relisting are held to: each spread rate from its launch rate
//! to twice it, and each margin requirement at or above its launch requirement.

use std::iter::once;

use alloy_primitives::U256;

use crate::Fixed;
use crate::market::Tier;
use crate::spread_tier::SpreadTier;

/// The rate of the band that charges the notional just above `notional`. `tiers` is a validated
/// table, whose last band is unbounded.
fn rate_above(tiers: &[SpreadTier], notional: Fixed) -> i64 {
    tiers.iter().find(|tier| tier.up_to.is_none_or(|up_to| up_to > notional)).map_or(0, SpreadTier::tenth_bps)
}

/// Checks that at every notional `tiers` charges at least `launch`'s rate there and at most twice
/// it: both are constant between the bounds of either table, so each stretch is checked once.
/// Error cases:
/// - a stretch where the rate is below the launch rate or above twice it
pub(crate) fn check_spread_within_launch(tiers: &[SpreadTier], launch: &[SpreadTier]) -> Result<(), String> {
    let starts = once(Fixed::ZERO).chain(tiers.iter().chain(launch).filter_map(|tier| tier.up_to));
    for start in starts {
        let (rate, launched) = (rate_above(tiers, start), rate_above(launch, start));
        if rate < launched || rate > launched * 2 {
            return Err(format!(
                "a spread rate of {rate} tenths of a bp above {start} USD is outside {launched}..={}, its launch rate to twice it",
                launched * 2
            ));
        }
    }
    Ok(())
}

/// The margin `tiers` require on `notional` before rounding, in raw units times bps: each band's
/// rate times the slice of the notional inside it. The sum is wide enough for any bound a table
/// validates.
fn exact_requirement(tiers: &[Tier], notional: Fixed) -> U256 {
    let mut low = Fixed::ZERO;
    tiers.iter().fold(U256::ZERO, |sum, tier| {
        let high = tier.up_to.map_or(notional, |up_to| up_to.min(notional));
        // Bounds are positive and increasing, so the slice is never negative.
        let slice = (high - low.min(high)).raw().unsigned_abs();
        low = low.max(high);
        sum + U256::from(slice) * U256::from(tier.bps.unsigned_abs())
    })
}

/// Checks that at every notional the margin `tiers` require is at least what `launch` requires,
/// before the rounding up the engine applies to each band's charge, which can differ by a raw unit
/// per band between tables. The unrounded requirements are linear between the bounds of either table,
/// so they are compared at each bound, and past the last by the last band's rate.
/// Error cases:
/// - a bound where the unrounded requirement is below launch's, or a last rate below launch's
pub(super) fn check_margin_at_or_above_launch(tiers: &[Tier], launch: &[Tier]) -> Result<(), String> {
    for bound in tiers.iter().chain(launch).filter_map(|tier| tier.up_to) {
        if exact_requirement(tiers, bound) < exact_requirement(launch, bound) {
            return Err(format!("the margin requirement at {bound} USD is below its launch requirement"));
        }
    }
    let last = |table: &[Tier]| table.last().map_or(0, |tier| tier.bps);
    if last(tiers) < last(launch) {
        return Err(format!("the margin rate past the last band is below its launch rate of {}bps", last(launch)));
    }
    Ok(())
}
