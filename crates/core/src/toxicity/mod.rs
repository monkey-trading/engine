//! From `V54`, the toxicity score: a stick that only ever adds to an account's spread, measured over
//! time so that neither round trips nor size dilute it. Every account starts on the best price.
//!
//! - **Markout.** A fill's markout is the index's move from the fill's index to the market's
//!   tradeable index `HORIZON_MS` after the fill, signed for the fill's side, in bps of the fill's
//!   index, rounded up and clamped to ±1000 bps; the spread paid does not enter it. A markout whose
//!   market has no tradeable index when it falls due waits for one, and one still waiting a horizon
//!   after it fell due is dropped unscored. Close-out fills are not marked.
//! - **Unit.** It is scored in standard errors of its market's markout spread (`spread`), rounded
//!   up.
//! - **Weight.** A fill's weight is its USD notional, capped at `WEIGHT_CAP` times the account's
//!   decayed mean fill notional (no cap on its first fill), and at `MAX_WEIGHT_USD`.
//! - **Score.** The account's edge sum `E`, each weight times its markout rounded up, its squared
//!   weights `W2`, its fill count and its fill notional decay together, half every `HALF_LIFE_MS`,
//!   in whole hours (`decay`). The score is `E / sqrt(W2)`: how many standard errors its recent edge
//!   sits above zero. From `MIN_FILLS` decayed fills, the highest tier whose `min_score` `z` passes
//!   `E >= 0 && E^2 >= z^2 W2`, in integers, adds its spread to every fill but a close-out.
//! - **Round trips** (`marking`, `legs`). A fill matches the account's opposite fills within the horizon
//!   newest-first: the quantity it closes against what they opened, and the quantity it opens
//!   against what they closed. Quantity it matches against a fill at the same index is a round trip:
//!   it adds nothing, on either fill, to the edge, the squared weights, the fill count or the fill
//!   notional; a fill partly matched counts the share of it left.
//! - **Bounds** (`bounds`). The horizon, half-life, fill minimum and weight cap are protocol. Tier
//!   thresholds and add-ons move only after a seven-day notice, within ±50% of launch, with an add-on
//!   of at least 1 bp; a market's floor is set at its listing and fixed after. The activation gives a
//!   market listed before it with no floor its asset class's (`default_markout_floor_bps`).

use std::ops::RangeInclusive;

use alloy_primitives::{U256, U512};
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::Engine;
use crate::fixed::Rounding;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, Millis};

mod bounds;
pub use bounds::default_markout_floor_bps;
mod decay;
mod legs;
pub(crate) use legs::MarkoutLegs;
mod marking;
pub(crate) use marking::FillMark;
mod spread;
pub use spread::{GRID_MS, MarkoutSpread, WINDOW_MS};

/// How long after a fill its markout falls due.
pub const HORIZON_MS: Millis = 5_000;
/// The score's half-life.
pub const HALF_LIFE_MS: Millis = 7 * 24 * 60 * 60 * 1000;
/// The decayed fills an account needs before any tier applies.
pub const MIN_FILLS: i64 = 20;
/// A fill's weight is at most this many times the account's decayed mean fill notional.
pub const WEIGHT_CAP: i64 = 4;
/// The most one fill weighs, in USD: an arithmetic bound that keeps the squared weights in range.
const MAX_WEIGHT_USD: Fixed = Fixed::MAX_INPUT;
/// The highest score a tier may start at, which keeps its square in range.
const MAX_MIN_SCORE: Fixed = Fixed::from_int(1_000);
/// The markout floors a listing may name, in bps: from a hundredth of a bp, which keeps a markout's
/// standard errors in range, to the price itself.
pub const MARKOUT_FLOOR_BPS: RangeInclusive<Fixed> =
    RangeInclusive::new(Fixed::from_raw(10_000_000), Fixed::from_int(10_000));

/// A score tier: from `min_score` standard errors up, `extra_spread_bps` on every fill but a
/// close-out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoreTier {
    pub min_score: Fixed,
    pub extra_spread_bps: i64,
}

/// An account's score as of a decay step: the decayed sums the score and its weight cap read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToxicityScore {
    /// `E`: the decayed sum of each scored fill's weight times its markout, in USD standard errors.
    pub edge: Fixed,
    /// `W2`: the decayed sum of the squared weights, in USD².
    pub weight_squares: Fixed,
    /// The decayed count of scored fills, matched quantity left out.
    pub fills: Fixed,
    /// The decayed sum of the scored fills' notional, in USD, whose mean caps a weight.
    pub notional: Fixed,
    /// The decay step, in whole hours of sequenced time, the sums are decayed to.
    pub step: i64,
}

/// An account's score as the account read gives it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ToxicityStanding {
    /// `E / sqrt(W2)`, rounded down; zero with no weight.
    pub score: Fixed,
    /// The decayed fills behind it.
    pub fills: Fixed,
    /// The tier that applies, counted from 1; 0 for none.
    pub tier: usize,
    /// The spread the tier adds, in bps.
    pub add_on_bps: i64,
}

/// The protocol's scoring constants, for the parameters read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ScoreRules {
    pub horizon_ms: Millis,
    pub half_life_ms: Millis,
    pub min_fills: i64,
    pub weight_cap: i64,
    pub grid_ms: Millis,
    pub window_ms: Millis,
}

/// `ScoreRules` as this build sets them.
pub const SCORE_RULES: ScoreRules = ScoreRules {
    horizon_ms: HORIZON_MS,
    half_life_ms: HALF_LIFE_MS,
    min_fills: MIN_FILLS,
    weight_cap: WEIGHT_CAP,
    grid_ms: GRID_MS,
    window_ms: WINDOW_MS,
};

/// The tiers `V54` activates with where the parameters name none: scores of 4 and 8 adding 2 and
/// 5 bps.
pub fn launch_score_tiers() -> Vec<ScoreTier> {
    [(4, 2), (8, 5)]
        .into_iter()
        .map(|(score, bps)| ScoreTier { min_score: Fixed::from_int(score), extra_spread_bps: bps })
        .collect()
}

/// Checks score tiers are strictly ascending, each from a positive score up to `MAX_MIN_SCORE`, and
/// each adds 1 to 10000 bps.
/// Error cases:
/// - a tier at or below the one before it
/// - a score at or below zero, or above `MAX_MIN_SCORE`
/// - an add-on outside 1..=10000 bps
pub fn validate_score_tiers(tiers: &[ScoreTier]) -> Result<(), String> {
    if tiers.windows(2).any(|pair| pair[0].min_score >= pair[1].min_score) {
        return Err("score tiers must be strictly ascending".into());
    }
    let bad = |tier: &ScoreTier| {
        !tier.min_score.is_positive()
            || tier.min_score > MAX_MIN_SCORE
            || !(1..=10_000).contains(&tier.extra_spread_bps)
    };
    if tiers.iter().any(bad) {
        return Err(format!("a score tier needs 0 < min_score <= {MAX_MIN_SCORE} and extra_spread_bps in 1..=10000"));
    }
    Ok(())
}

/// `a + b`, saturating at `Fixed`'s range: a decayed sum that reached it would stay there.
fn saturating(a: Fixed, b: Fixed) -> Fixed {
    Fixed::from_raw(a.raw().saturating_add(b.raw()))
}

impl ToxicityScore {
    /// The sums decayed to the step `ts` falls in.
    fn decayed_to(&self, ts: Millis) -> ToxicityScore {
        let step = decay::step_of(ts);
        let steps = step - self.step;
        ToxicityScore {
            edge: decay::decay(self.edge, steps),
            weight_squares: decay::decay(self.weight_squares, steps),
            fills: decay::decay(self.fills, steps),
            notional: decay::decay(self.notional, steps),
            step: step.max(self.step),
        }
    }

    /// Adds a scored fill of `notional` USD, `share` of a whole fill, with a markout of `markout`
    /// standard errors, to sums already decayed to now.
    fn fold(&mut self, notional: Fixed, share: Fixed, markout: Fixed) {
        let notional = notional.min(MAX_WEIGHT_USD);
        let cap = self
            .fills
            .is_positive()
            .then(|| self.notional.mul_div(Fixed::from_int(WEIGHT_CAP), self.fills, Rounding::Down));
        let weight = cap.map_or(notional, |cap| notional.min(cap));
        self.edge = saturating(self.edge, weight.mul_rounded(markout, Rounding::Up));
        self.weight_squares = saturating(self.weight_squares, weight.mul(weight));
        self.fills = saturating(self.fills, share);
        self.notional = saturating(self.notional, notional);
    }

    /// The index of the highest of `tiers` the sums reach, or `None` before `MIN_FILLS` fills, with
    /// no weight, or below the first. A tier's `z` is reached when `E >= 0 && E^2 >= z^2 W2`, which
    /// compares `E^2 * 10^9` with `z^2 * W2` in the raw units, in 512 bits.
    fn tier(&self, tiers: &[ScoreTier]) -> Option<usize> {
        let edge = u128::try_from(self.edge.raw()).ok()?;
        let weight_squares = u128::try_from(self.weight_squares.raw()).ok().filter(|w2| *w2 > 0)?;
        if self.fills < Fixed::from_int(MIN_FILLS) {
            return None;
        }
        let lhs = U512::from(edge) * U512::from(edge) * U512::from(Fixed::ONE.raw() as u128);
        tiers.iter().rposition(|tier| {
            // Validated scores are positive.
            let z = U512::from(tier.min_score.raw() as u128);
            lhs >= z * z * U512::from(weight_squares)
        })
    }

    /// `E / sqrt(W2)`, rounded down, or zero with no weight.
    fn score(&self) -> Fixed {
        if !self.weight_squares.is_positive() {
            return Fixed::ZERO;
        }
        // sqrt(W2) in raw units: the root of the raw value scaled once more, which is at most
        // 2^128 * 10^9 and so has a root below 2^128.
        let scaled = U256::from(self.weight_squares.raw() as u128) * U256::from(Fixed::ONE.raw() as u128);
        let root = Fixed::from_raw(i128::try_from(isqrt(scaled)).expect("the root fits 128 bits"));
        self.edge.mul_div(Fixed::ONE, root, Rounding::Down)
    }
}

/// The integer square root of `n`, rounded down, by Newton's method from a power of two at or above
/// the root, which falls to the root and stops there.
fn isqrt(n: U256) -> U256 {
    if n.is_zero() {
        return n;
    }
    let mut root = U256::ONE << n.bit_len().div_ceil(2);
    loop {
        let next = (root + n / root) >> 1;
        if next >= root {
            return root;
        }
        root = next;
    }
}

impl Engine {
    /// The spread `account`'s toxicity adds to its next fill but a close-out, in bps: from `V54` its
    /// score's tier's, and before it the tier its `V11` score reaches.
    pub fn toxicity_add_on_bps(&self, account: AccountId) -> i64 {
        let Some(params) = &self.params else { return 0 };
        if self.protocol_version < ProtocolVersion::V54 {
            return self.accounts.get(&account).map_or(0, |acct| params.toxicity_spread_bps(acct.toxicity_bps));
        }
        let Some(score) = self.toxicity_scores.get(&account) else { return 0 };
        let tiers = self.score_tiers();
        score.decayed_to(self.now).tier(tiers).map_or(0, |index| tiers[index].extra_spread_bps)
    }

    /// From `V54`, `account`'s score, its decayed fills, its tier and the spread the tier adds, as of
    /// the sequenced time; neutral for an account never scored. `None` before `V54`.
    pub fn toxicity_standing(&self, account: AccountId) -> Option<ToxicityStanding> {
        if self.protocol_version < ProtocolVersion::V54 {
            return None;
        }
        let Some(score) = self.toxicity_scores.get(&account) else { return Some(ToxicityStanding::default()) };
        let score = score.decayed_to(self.now);
        let tiers = self.score_tiers();
        let tier = score.tier(tiers);
        Some(ToxicityStanding {
            score: score.score(),
            fills: score.fills,
            tier: tier.map_or(0, |index| index + 1),
            add_on_bps: tier.map_or(0, |index| tiers[index].extra_spread_bps),
        })
    }

    /// The score tiers in force; none before parameters are set.
    fn score_tiers(&self) -> &[ScoreTier] {
        self.params.as_ref().map_or(&[], |params| &params.toxicity.score_tiers)
    }
}

#[cfg(test)]
mod tests;
