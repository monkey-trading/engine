//! A group's loading-weighted exposure for one account, by side, and the credit its matched part takes
//! off the per-position requirement, worked exactly in wide integers and rounded once.
//!
//! The range argument. A leg's notional |qty × price| is formed as a `Fixed` before it enters here
//! (halting outside its range, as every margin read does), so its raw value is below 2^127, and
//! |qty × price| at `Fixed`'s scale squared is below 2^127 × 10^9 < 2^157. A loading is at most
//! `MAX_LOADING`, 10^10 < 2^34 raw, and a group has at most one member per market id, 2^32, so a side's
//! exposure stays below 2^223. A side's bands are below 2^127, since their sum over every side is the
//! per-position requirement, a `Fixed`. A leg's grid charge is its notional times at most 10^4 × 10^4
//! plus 10^4 × 10^4 (`Engine::charge`'s scale), below 2^155, and a side's below 2^187. So `credit`'s
//! largest product, 10^4 × (bands × exposure × 10^8 + charge × exposure), is below 2^425, inside
//! `U512`.

use alloy_primitives::U512;

use crate::Fixed;

/// The raw value of a product of three `Fixed` values is at `Fixed`'s scale cubed: this over it
/// brings it to `Fixed`'s.
const SCALE_SQUARED: u64 = 1_000_000_000_000_000_000;
pub(super) const BPS: u64 = 10_000;

/// One position's part in its group's exposure.
pub(super) struct Leg {
    pub(super) qty: Fixed,
    pub(super) price: Fixed,
    pub(super) loading: Fixed,
    /// Its per-position requirement.
    pub(super) bands: Fixed,
    /// Its grid charge, its own move and the cost of closing it, at `Fixed`'s scale times the charge
    /// scale `Exposure::credit` takes.
    pub(super) charge: U512,
    /// Its own move, ρ × notional, at `Fixed`'s scale times 10^4.
    pub(super) residual: U512,
}

/// One side, long or short, of a group's loading-weighted exposure: sums over its legs.
#[derive(Default)]
struct Side {
    /// Σ |β × qty × price|, at `Fixed`'s scale cubed.
    exposure: U512,
    bands: U512,
    charge: U512,
    residual: U512,
}

/// A group's loading-weighted exposure for one account, its long and its short side summed apart.
#[derive(Default)]
pub(super) struct Exposure {
    long: Side,
    short: Side,
}

impl Exposure {
    /// Adds `leg` to the side its loading-weighted exposure is on.
    pub(super) fn add(&mut self, leg: Leg) {
        let magnitude = [leg.qty, leg.price, leg.loading]
            .iter()
            .fold(U512::from(1), |product, factor| product.saturating_mul(U512::from(factor.raw().unsigned_abs())));
        let negative = leg.qty.is_negative() ^ leg.price.is_negative() ^ leg.loading.is_negative();
        let side = if negative { &mut self.short } else { &mut self.long };
        side.exposure = side.exposure.saturating_add(magnitude);
        side.bands += wide(leg.bands);
        side.charge += leg.charge;
        side.residual += leg.residual;
    }

    /// Whether the net is below zero, and its magnitude at `Fixed`'s scale, rounded up.
    pub(super) fn net(&self) -> (bool, U512) {
        let short = self.short.exposure > self.long.exposure;
        let magnitude = self.long.exposure.abs_diff(self.short.exposure);
        (short, ceil_div(magnitude, U512::from(SCALE_SQUARED)))
    }

    /// The matched exposure, the gross less the net's magnitude (twice the smaller side), at `Fixed`'s
    /// scale, rounded down.
    pub(super) fn matched(&self) -> U512 {
        let (smaller, _) = self.sides();
        smaller.exposure * U512::from(2) / U512::from(SCALE_SQUARED)
    }

    /// The matched exposure's worst loss over the grid, at `Fixed`'s scale, rounded up: each matched
    /// part's own move, the factor's cancelling. The larger side is matched in the ratio of the
    /// smaller to it.
    pub(super) fn worst_loss(&self) -> U512 {
        let (smaller, larger) = self.sides();
        let (matched, whole) = (smaller.exposure, larger.exposure);
        if matched.is_zero() {
            return U512::ZERO;
        }
        let loss = smaller.residual * whole + matched * larger.residual;
        ceil_div(loss, whole * U512::from(BPS))
    }

    /// The credit the matched exposure takes off the per-position requirement, exactly, rounded down
    /// once: its share of the bands less its grid charge, at most `1 - φ` of that share, and never below
    /// zero. `charge_scale` is the scale each leg's charge is at over `Fixed`'s. Nothing when either
    /// side is empty.
    pub(super) fn credit(&self, floor_share_bps: i64, charge_scale: U512) -> Fixed {
        let (smaller, larger) = self.sides();
        let (matched, whole) = (smaller.exposure, larger.exposure);
        if matched.is_zero() {
            return Fixed::ZERO;
        }
        // The matched part's bands and grid charge, each times the larger side's exposure, the charge
        // at its own scale.
        let bands = (smaller.bands * whole + matched * larger.bands) * charge_scale;
        let charge = smaller.charge * whole + matched * larger.charge;
        if bands <= charge {
            return Fixed::ZERO;
        }
        let kept = U512::from(BPS - floor_share_bps.unsigned_abs());
        let credit = (kept * bands).min(U512::from(BPS) * (bands - charge));
        let credit = credit / (whole * charge_scale * U512::from(BPS));
        Fixed::from_raw(i128::try_from(credit).expect("a credit is at most the bands of a side, a Fixed"))
    }

    /// The smaller side and the larger.
    fn sides(&self) -> (&Side, &Side) {
        if self.short.exposure < self.long.exposure { (&self.short, &self.long) } else { (&self.long, &self.short) }
    }
}

/// A non-negative `Fixed` as a wide amount at its scale.
pub(super) fn wide(amount: Fixed) -> U512 {
    U512::from(amount.raw().unsigned_abs())
}

/// A wide amount as a `Fixed`, at most the largest one: a figure past it reads as it.
pub(super) fn narrow(amount: U512) -> Fixed {
    Fixed::from_raw(i128::try_from(amount).unwrap_or(i128::MAX))
}

/// `numerator / denominator`, rounded up. Panics on a zero denominator, which callers rule out.
pub(super) fn ceil_div(numerator: U512, denominator: U512) -> U512 {
    let (quotient, remainder) = numerator.div_rem(denominator);
    if remainder.is_zero() { quotient } else { quotient + U512::from(1) }
}
