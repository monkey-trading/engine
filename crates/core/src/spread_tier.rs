//! Size spread bands. From `V52` a band's rate is in tenths of a basis point and no band is below its
//! asset class's floor, so no market quotes a zero spread. A band listed before `V52` is written in
//! whole bps, or in tenths, and keeps that encoding, so earlier logs and signed listings encode as
//! they did, until `V52`'s activation writes it in tenths.

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::fixed::Rounding;
use crate::market::{banded, validate_bounds};
use crate::protocol::ProtocolVersion;
use crate::types::AssetClass;

/// The most a band written in whole bps may charge: 100%.
const MAX_BPS: i64 = 10_000;
/// The most a band written in tenths of a bp may charge: 100%.
const MAX_TENTH_BPS: i64 = 100_000;

/// A size spread band: its rate applies to the slice of position notional (USD) from the band
/// before's bound up to `up_to`. `up_to: None` means unbounded and must be last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SpreadTierEncoding", into = "SpreadTierEncoding")]
pub struct SpreadTier {
    pub up_to: Option<Fixed>,
    /// The rate as written: whole bps when `whole_bps`, tenths of a bp otherwise.
    rate: i64,
    whole_bps: bool,
}

/// How a band is written: its rate in whole bps (`bps`) before `V52`, and in tenths of a bp
/// (`tenth_bps`) from it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpreadTierEncoding {
    #[serde(default)]
    up_to: Option<Fixed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bps: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tenth_bps: Option<i64>,
}

impl TryFrom<SpreadTierEncoding> for SpreadTier {
    type Error = String;

    /// The band its encoding writes.
    /// Error cases:
    /// - neither `bps` nor `tenth_bps`, or both
    fn try_from(encoding: SpreadTierEncoding) -> Result<Self, String> {
        let up_to = encoding.up_to;
        match (encoding.bps, encoding.tenth_bps) {
            (Some(bps), None) => Ok(SpreadTier::bps(up_to, bps)),
            (None, Some(tenth_bps)) => Ok(SpreadTier::tenths(up_to, tenth_bps)),
            _ => Err("a spread band names its rate once, as bps or as tenth_bps".into()),
        }
    }
}

impl From<SpreadTier> for SpreadTierEncoding {
    fn from(tier: SpreadTier) -> Self {
        let (bps, tenth_bps) = if tier.whole_bps { (Some(tier.rate), None) } else { (None, Some(tier.rate)) };
        SpreadTierEncoding { up_to: tier.up_to, bps, tenth_bps }
    }
}

impl SpreadTier {
    /// A band written in whole bps, as every listing before `V52` is.
    pub const fn bps(up_to: Option<Fixed>, bps: i64) -> Self {
        SpreadTier { up_to, rate: bps, whole_bps: true }
    }

    /// A band written in tenths of a bp, as every listing from `V52` is.
    pub const fn tenths(up_to: Option<Fixed>, tenth_bps: i64) -> Self {
        SpreadTier { up_to, rate: tenth_bps, whole_bps: false }
    }

    /// The band's rate in tenths of a bp, however it is written.
    pub fn tenth_bps(&self) -> i64 {
        if self.whole_bps { self.rate.saturating_mul(10) } else { self.rate }
    }

    /// Whether the band is written in whole bps.
    pub fn is_whole_bps(&self) -> bool {
        self.whole_bps
    }

    /// The band in tenths of a bp, raised to `floor`: how `V52`'s activation converts a listing.
    pub(crate) fn converted(self, floor: i64) -> Self {
        SpreadTier::tenths(self.up_to, self.tenth_bps().max(floor))
    }
}

/// The least rate a spread band of a market of `class` may have from `V52`, in tenths of a bp: a
/// tenth of a bp on FX and index markets, and 1bp on every other class.
pub fn spread_floor_tenth_bps(class: AssetClass) -> i64 {
    match class {
        AssetClass::Fx | AssetClass::Index => 1,
        _ => 10,
    }
}

/// Checks a spread band table: non-empty, strictly increasing bounds, an unbounded last band, and
/// every rate from zero to 100%.
/// Error cases:
/// - an empty table, a bounded last band, an unbounded band before the last, or a bound that does
///   not exceed the previous one
/// - a whole-bps rate outside 0..=10000, or a rate in tenths outside 0..=100000
pub(crate) fn validate_spread_tiers(name: &str, tiers: &[SpreadTier]) -> Result<(), String> {
    validate_bounds(name, &tiers.iter().map(|tier| tier.up_to).collect::<Vec<_>>())?;
    if tiers.iter().any(|tier| tier.whole_bps && !(0..=MAX_BPS).contains(&tier.rate)) {
        return Err(format!("{name}: rates must be in 0..={MAX_BPS} bps"));
    }
    if tiers.iter().any(|tier| !tier.whole_bps && !(0..=MAX_TENTH_BPS).contains(&tier.rate)) {
        return Err(format!("{name}: rates must be in 0..={MAX_TENTH_BPS} tenths of a bp"));
    }
    Ok(())
}

/// Checks spread bands as `version` lists them: from `V52` in tenths of a bp, each at least the floor
/// of `class`. Before `V52` either unit lists, at the same rate, and no floor binds.
/// Error cases:
/// - from `V52`, a band in whole bps, or one below `class`'s floor
pub fn check_spread_tiers(tiers: &[SpreadTier], class: AssetClass, version: ProtocolVersion) -> Result<(), String> {
    if version < ProtocolVersion::V52 {
        return Ok(());
    }
    if tiers.iter().any(|tier| tier.whole_bps) {
        return Err(format!("from protocol {:?} spread bands are in tenths of a bp (tenth_bps)", ProtocolVersion::V52));
    }
    let floor = spread_floor_tenth_bps(class);
    if let Some(low) = tiers.iter().find(|tier| tier.rate < floor) {
        return Err(format!(
            "a spread band of {} tenths of a bp is below the {floor} tenths floor of a {class:?} market",
            low.rate
        ));
    }
    Ok(())
}

/// Cost of moving from notional `from` to `to` (`from <= to`) through spread bands: each band's rate
/// applies to the slice of `[from, to]` inside it, and each band's charge rounds as `rounding` says.
pub fn spread_staircase(tiers: &[SpreadTier], from: Fixed, to: Fixed, rounding: Rounding) -> Fixed {
    banded(tiers.iter().map(|tier| (tier.up_to, tier.tenth_bps())), from, to, |slice, rate| {
        slice.mul_tenth_bps_rounded(rate, rounding)
    })
}

#[cfg(test)]
mod tests;
