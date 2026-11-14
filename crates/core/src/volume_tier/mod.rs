//! Volume tiers, from `V59`: an account earns a tier from its counted volume over the last
//! `WINDOW_DAYS` UTC days of sequenced time, and a higher tier trades more size at the best price.
//! Every account starts at tier 0.
//!
//! - **Counting** (`book`). Opening notional is pending until it has been held `MIN_HOLD_MS`, then
//!   counts on the UTC day it matures. A close consumes the position's pending notional newest first,
//!   and that part counts on neither fill. A close of matured notional counts on the closing fill's
//!   day once the toxicity score's horizon has passed, unless the account reopens it at the same
//!   index within the horizon, a round trip (`toxicity`): the quantity reopened is matured again and
//!   counts nothing on either fill. The book keeps each close's leg as the score does, so a reopening
//!   at another index uses it up as the score does. A close-out, which no round trip matches, counts at once. Only
//!   counted volume reaches a tier, so pending volume never grants a benefit.
//! - **Tiers.** A day counts toward a tier at most its threshold over its day divisor, and the tier is
//!   the highest whose capped sum over the window reaches its threshold; tier 0 needs nothing.
//! - **Best price.** An account's first spread band ends at its tier's share of its market's first
//!   bound, rounded down, and at most the second bound. No tier changes a rate.
//! - **Bounds** (`bounds`). The table moves only after a seven-day notice, or by a vote while rotation
//!   is on: the same number of tiers, thresholds and day divisors at or above launch, shares at or
//!   below launch, and in every listed market each tier's spread at every notional from its launch
//!   spread to twice it, against which a listing's spread bands are checked too.
//! - **Activation.** Parameters that name no table take `launch_volume_tiers`, and every account
//!   starts at tier 0 with no counted volume; every position held then is pending for its first
//!   `MIN_HOLD_MS`, as no record says which of its units were held that long. Activation is refused
//!   over a listing outside a tier's bound, and a market's first listing is held to the same bound at
//!   its own bands.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::Engine;
use crate::fixed::Rounding;
use crate::market::banded;
use crate::protocol::ProtocolVersion;
use crate::spread_tier::SpreadTier;
use crate::types::{AccountId, MarketId, Millis};

mod book;
pub use book::{AccountVolume, ClosingLot, DayVolume, MAX_CLOSING_LOTS, MAX_OPENING_LOTS, OpeningLot};
mod bounds;
pub(crate) use bounds::spend_volume_tier_notice;

/// How long opening notional is held before it counts: 60 s of sequenced time.
pub const MIN_HOLD_MS: Millis = 60_000;
/// The UTC days of sequenced time counted volume is kept for, the current one included.
pub const WINDOW_DAYS: usize = 30;
/// A UTC day of sequenced time.
pub const DAY_MS: Millis = 24 * 60 * 60 * 1000;
/// The most tiers a table may hold.
pub const MAX_VOLUME_TIERS: usize = 8;
/// The largest best-price share a tier may have: ten times the first bound.
const MAX_SHARE_BPS: i64 = 100_000;

/// One volume tier: the counted volume that reaches it, how many days that takes, and its benefit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeTier {
    /// The counted volume over the window, in USD, that reaches the tier; zero for tier 0.
    pub threshold: Fixed,
    /// A day counts at most `threshold / day_divisor` towards the tier, so it takes counted volume on
    /// at least this many days; 1 for tier 0.
    pub day_divisor: i64,
    /// Where the account's first spread band ends, in bps of its market's first bound.
    pub best_price_share_bps: i64,
}

/// An account's volume as the account read gives it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct VolumeStanding {
    /// The tier the account trades at now.
    pub tier: usize,
    /// The volume counted over the window, in USD.
    pub matured: Fixed,
    /// The notional waiting to count, in USD: opening notional held under `MIN_HOLD_MS`, and closes
    /// within the round-trip horizon.
    pub pending: Fixed,
    /// Each tier's progress, tier 0 first.
    pub progress: Vec<TierProgress>,
    /// Where each market's first spread band ends for the account, in USD; a market with a single
    /// band is left out.
    pub best_price_bounds: BTreeMap<MarketId, Fixed>,
}

/// How far an account is towards one tier.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TierProgress {
    /// The counted volume the tier's day cap admits, which reaches it at its threshold.
    pub capped: Fixed,
    /// The days in the window whose counted volume reaches the tier's day cap.
    pub days: usize,
}

/// A market's spread bands as one account pays them: its first band ends at the account's
/// best-price bound.
#[derive(Clone, Copy, Debug)]
pub struct AccountBands<'a> {
    tiers: &'a [SpreadTier],
    first_up_to: Option<Fixed>,
}

impl<'a> AccountBands<'a> {
    /// `tiers` with the first band ending at `share_bps` of its bound, rounded down, and at most the
    /// second band's bound; a table of one band is unchanged. A share above the whole bound never takes
    /// it past the larger of the bound and `Fixed::MAX_INPUT` of notional, so it is never larger than a
    /// bound a listing could name beside it.
    pub fn new(tiers: &'a [SpreadTier], share_bps: i64) -> Self {
        let first_up_to = match tiers {
            [first, second, ..] => first.up_to.map(|bound| {
                let (share, ceiling) = (Fixed::from_raw(share_bps.into()), bound.max(Fixed::MAX_INPUT));
                let scaled = bound.checked_mul_div(share, Fixed::from_raw(10_000), Rounding::Down);
                let scaled = scaled.map_or(ceiling, |scaled| scaled.min(ceiling));
                second.up_to.map_or(scaled, |next| scaled.min(next))
            }),
            _ => tiers.first().and_then(|tier| tier.up_to),
        };
        AccountBands { tiers, first_up_to }
    }

    /// `tiers` as an account at `share` pays them (`new`), or as listed with no share.
    pub(crate) fn at(tiers: &'a [SpreadTier], share: Option<i64>) -> Self {
        match share {
            Some(share) => AccountBands::new(tiers, share),
            None => AccountBands { tiers, first_up_to: tiers.first().and_then(|tier| tier.up_to) },
        }
    }

    /// Each band's bound and rate, in tenths of a bp.
    fn bands(&self) -> impl Iterator<Item = (Option<Fixed>, i64)> + Clone + 'a {
        let first_up_to = self.first_up_to;
        self.tiers
            .iter()
            .enumerate()
            .map(move |(at, tier)| (if at == 0 { first_up_to } else { tier.up_to }, tier.tenth_bps()))
    }

    /// The cost of moving from notional `from` to `to` through the bands, as `spread_staircase`
    /// prices it.
    pub fn staircase(&self, from: Fixed, to: Fixed, rounding: Rounding) -> Fixed {
        banded(self.bands(), from, to, |slice, rate| slice.mul_tenth_bps_rounded(rate, rounding))
    }

    /// Where the first band ends; `None` for a table of one band.
    pub fn best_price_bound(&self) -> Option<Fixed> {
        self.first_up_to.filter(|_| self.tiers.len() > 1)
    }

    /// The bands' bounds, strictly increasing: a second band the first now reaches the end of is
    /// left out.
    pub fn bounds(&self) -> Vec<Fixed> {
        let mut bounds: Vec<Fixed> = Vec::with_capacity(self.tiers.len());
        for bound in self.bands().filter_map(|(up_to, _)| up_to) {
            if bounds.last().is_none_or(|last| bound > *last) {
                bounds.push(bound);
            }
        }
        bounds
    }

    /// The bands as a table, for comparing rates at every notional.
    pub fn tiers(&self) -> Vec<SpreadTier> {
        let mut tiers = self.tiers.to_vec();
        if let Some(first) = tiers.first_mut() {
            first.up_to = self.first_up_to;
        }
        tiers
    }
}

/// The table `V59` activates with where the parameters name none: tier 0 at a quarter of the first
/// bound, then $1M over two days or more at half, $25M over five days at the whole bound, and $250M
/// over ten days at two and a half times it.
pub fn launch_volume_tiers() -> Vec<VolumeTier> {
    [(0, 1, 2_500), (1_000_000, 2, 5_000), (25_000_000, 5, 10_000), (250_000_000, 10, 25_000)]
        .into_iter()
        .map(|(usd, day_divisor, best_price_share_bps)| VolumeTier {
            threshold: Fixed::from_int(usd),
            day_divisor,
            best_price_share_bps,
        })
        .collect()
}

/// Checks a tier table: none, or at most `MAX_VOLUME_TIERS` tiers, the first needing nothing (a zero
/// threshold and a day divisor of 1), thresholds strictly ascending up to `Fixed::MAX_INPUT`, day
/// divisors from 1 to `WINDOW_DAYS`, and shares from 1 to 100000 bps that never fall with the tier.
/// Error cases:
/// - more than `MAX_VOLUME_TIERS` tiers
/// - a first tier with a threshold or a day divisor that needs anything
/// - thresholds not strictly ascending, or one above `Fixed::MAX_INPUT`
/// - a day divisor outside 1..=30, past which the window could never reach the tier
/// - a share outside 1..=100000 bps, or one below the share of the tier before it
pub fn validate_volume_tiers(tiers: &[VolumeTier]) -> Result<(), String> {
    if tiers.len() > MAX_VOLUME_TIERS {
        return Err(format!("volume_tiers has more than {MAX_VOLUME_TIERS} tiers"));
    }
    if tiers.first().is_some_and(|tier| !tier.threshold.is_zero() || tier.day_divisor != 1) {
        return Err("volume tier 0 needs a threshold of 0 and a day_divisor of 1".into());
    }
    if tiers.iter().any(|tier| tier.threshold > Fixed::MAX_INPUT)
        || tiers.windows(2).any(|pair| pair[0].threshold >= pair[1].threshold)
    {
        return Err(format!("volume tier thresholds must be strictly ascending, up to {}", Fixed::MAX_INPUT));
    }
    if tiers.iter().any(|tier| !(1..=WINDOW_DAYS as i64).contains(&tier.day_divisor)) {
        return Err(format!("a volume tier's day_divisor must be in 1..={WINDOW_DAYS}"));
    }
    if tiers.iter().any(|tier| !(1..=MAX_SHARE_BPS).contains(&tier.best_price_share_bps))
        || tiers.windows(2).any(|pair| pair[0].best_price_share_bps > pair[1].best_price_share_bps)
    {
        return Err(format!("best_price_share_bps must be in 1..={MAX_SHARE_BPS} and never fall with the tier"));
    }
    Ok(())
}

/// The UTC day of sequenced time `ms` falls on, counted from the epoch.
pub fn day_of(ms: Millis) -> i64 {
    ms.div_euclid(DAY_MS)
}

/// A day's `volume` towards `tier`, times its day divisor so no cap is rounded: at most the threshold,
/// in raw units.
fn day_share(volume: Fixed, tier: &VolumeTier) -> i128 {
    volume.raw().saturating_mul(tier.day_divisor.into()).min(tier.threshold.raw())
}

/// The counted volume `window` holds towards `tier`, each day's at most the tier's day cap, times its
/// day divisor: exact, so a cap the divisor does not divide still sums to the threshold.
fn capped_times_divisor(window: &[Fixed; WINDOW_DAYS], tier: &VolumeTier) -> i128 {
    window.iter().fold(0i128, |sum, volume| sum.saturating_add(day_share(*volume, tier)))
}

/// Whether the counted volume in `window` reaches `tier`.
fn reaches(window: &[Fixed; WINDOW_DAYS], tier: &VolumeTier) -> bool {
    // A validated threshold is at most `Fixed::MAX_INPUT` and a divisor at most 30, so this fits.
    capped_times_divisor(window, tier) >= tier.threshold.raw() * i128::from(tier.day_divisor)
}

/// The highest tier of `tiers` whose capped volume in `window` reaches its threshold.
fn tier_of(window: &[Fixed; WINDOW_DAYS], tiers: &[VolumeTier]) -> usize {
    tiers.iter().rposition(|tier| reaches(window, tier)).unwrap_or(0)
}

impl Engine {
    /// The tier table in force: from `V59`, the parameters'; `None` before it.
    pub(crate) fn volume_tiers_in_force(&self) -> Option<&[VolumeTier]> {
        let tiers = &self.params.as_ref()?.volume_tiers;
        (self.protocol_version >= ProtocolVersion::V59 && !tiers.is_empty()).then_some(tiers.as_slice())
    }

    /// The volume tier `account` trades at now: from `V59`, the highest its counted volume reaches,
    /// and 0 before it.
    pub fn volume_tier(&self, account: AccountId) -> usize {
        let (Some(tiers), Some(volume)) = (self.volume_tiers_in_force(), self.volumes.get(&account)) else {
            return 0;
        };
        tier_of(&volume.window(self.hold_clock_time()), tiers)
    }

    /// `account`'s volume book, as last changed: a lot whose time has come counts on its day when the
    /// book is next changed, and every read before then counts it (`volume_standing`).
    pub fn account_volume(&self, account: AccountId) -> Option<&AccountVolume> {
        self.volumes.get(&account)
    }

    /// The share of each market's first bound `account`'s first band ends at now, in bps: from `V59`
    /// its tier's, and `None` before it, when every band is as listed.
    pub(crate) fn best_price_share(&self, account: AccountId) -> Option<i64> {
        // The tier is below the table's length, which `volume_tiers_in_force` keeps above zero.
        self.volume_tiers_in_force().map(|table| table[self.volume_tier(account)].best_price_share_bps)
    }

    /// `account`'s volume, tier, progress towards each tier and best-price bound in each market, from
    /// `V59`; `None` before it.
    pub fn volume_standing(&self, account: AccountId) -> Option<VolumeStanding> {
        let tiers = self.volume_tiers_in_force()?;
        let now = self.hold_clock_time();
        let volume = self.volumes.get(&account).cloned().unwrap_or_default();
        let window = volume.window(now);
        let tier = tier_of(&window, tiers);
        let progress = tiers
            .iter()
            .map(|level| {
                let at_cap = |day: &&Fixed| day.is_positive() && day_share(**day, level) == level.threshold.raw();
                let capped = Fixed::from_raw(capped_times_divisor(&window, level) / i128::from(level.day_divisor));
                TierProgress { capped, days: window.iter().filter(at_cap).count() }
            })
            .collect();
        let share = tiers[tier].best_price_share_bps;
        let best_price_bounds = self
            .markets
            .iter()
            .filter_map(|(id, market)| {
                AccountBands::new(&market.spec.spread_tiers, share).best_price_bound().map(|bound| (*id, bound))
            })
            .collect();
        Some(VolumeStanding {
            tier,
            matured: window.iter().copied().sum(),
            pending: volume.pending(now),
            progress,
            best_price_bounds,
        })
    }
}

#[cfg(test)]
mod tests;
