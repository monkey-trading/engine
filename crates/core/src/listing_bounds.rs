//! Listing bounds, from `V52`: a listed market's spread and margin bands move only as a listing
//! notice of at least seven days names them, and only within bounds its launch bands set.
//!
//! A notice may name a market's spread bands, margin bands and financing markup (`V49`'s markup
//! notice is the notice naming only a markup), and a relisting applies each named value once, from
//! the notice's time on. At every notional a noticed spread rate is at least the launch rate there
//! and at most twice it, and a noticed margin requirement at least launch's. The launch bands are the
//! listing in force when `V52` activates, each spread rate raised to its class's floor, or a market's
//! first listing after it. A new listing needs no notice.

use std::iter::once;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::Engine;
use crate::fixed::Rounding;
use crate::market::{MarketSpec, Tier, staircase, validate_tiers};
use crate::params::MAX_RATE_BPS;
use crate::protocol::ProtocolVersion;
use crate::spread_tier::{SpreadTier, check_spread_tiers, validate_spread_tiers};
use crate::types::{MarketId, Millis};

/// The operator's notice of one listed market's new listing values: from `V49` its financing markup,
/// and from `V52` its spread and margin bands. A relisting may apply each named value from
/// `effective_at` on, once. A notice naming only a markup encodes as `V49`'s did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListingNotice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread_tiers: Option<Vec<SpreadTier>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin_tiers: Option<Vec<Tier>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markup_bps: Option<i64>,
    /// The sequenced time the named values may apply from.
    pub effective_at: Millis,
}

/// A market's launch bands, which bound the bands a listing notice may name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchBands {
    pub spread_tiers: Vec<SpreadTier>,
    pub margin_tiers: Vec<Tier>,
}

/// What a listing changes that its market's notice names, so applying it spends those values, and
/// whether it is the market's first listing.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ListingChange {
    first: bool,
    spread: bool,
    margin: bool,
    markup: bool,
}

impl ListingNotice {
    /// A notice of `markup_bps` alone, as `AnnounceMarkup` gives.
    pub fn markup(markup_bps: i64, effective_at: Millis) -> Self {
        ListingNotice { spread_tiers: None, margin_tiers: None, markup_bps: Some(markup_bps), effective_at }
    }

    /// Whether the notice names no value left to apply.
    fn names_nothing(&self) -> bool {
        self.spread_tiers.is_none() && self.margin_tiers.is_none() && self.markup_bps.is_none()
    }
}

impl Engine {
    /// On an activation carrying `version` to `V52` or later from before it: writes every listed
    /// market's spread bands in tenths of a bp, each raised to its class's floor, takes the result
    /// and the margin bands as the market's launch bands, and raises each stress level's size
    /// multiplier to at least 1x.
    pub(crate) fn start_listing_bounds(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V52 || version < ProtocolVersion::V52 {
            return;
        }
        let ids: Vec<MarketId> = self.markets.keys().copied().collect();
        for id in ids {
            let spec = &mut self.markets.get_mut(&id).expect("listed").spec;
            spec.convert_spread_tiers();
            let launch =
                LaunchBands { spread_tiers: spec.spread_tiers.clone(), margin_tiers: spec.margin_tiers.clone() };
            self.operator_bounds.launch_bands.insert(id, launch);
        }
        if let Some(params) = self.params.as_mut() {
            params.raise_size_multipliers();
        }
    }

    /// From `V52`, records a notice of `market`'s new listing values, replacing any pending one for
    /// that market.
    /// Error cases:
    /// - before `V52`, or for a market that is not listed
    /// - a notice naming nothing
    /// - spread bands that fail `MarketSpec::validate`'s table checks or `check_spread_tiers`, or
    ///   whose rate at some notional is below the launch rate there or above twice it
    /// - margin bands that fail the table checks, or whose requirement at some notional is below
    ///   launch's
    /// - a markup outside ±10000 bps a year
    /// - an effective time less than seven days after the sequenced time, or past the latest stamp
    pub(crate) fn announce_listing(&mut self, market: MarketId, notice: &ListingNotice) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V52 {
            return Err(format!("a listing notice needs protocol {:?}", ProtocolVersion::V52));
        }
        let listed = self.markets.get(&market).ok_or_else(|| format!("market {market} is not listed"))?;
        if notice.names_nothing() {
            return Err("a listing notice names nothing".into());
        }
        let launch = self
            .operator_bounds
            .launch_bands
            .get(&market)
            .expect("V52's activation records every market listed then, and each first listing after it");
        if let Some(tiers) = &notice.spread_tiers {
            validate_spread_tiers("spread_tiers", tiers)?;
            check_spread_tiers(tiers, listed.spec.asset_class, self.protocol_version)?;
            check_spread_within_launch(tiers, &launch.spread_tiers)?;
        }
        if let Some(tiers) = &notice.margin_tiers {
            validate_tiers("margin_tiers", tiers, 1)?;
            check_margin_at_or_above_launch(tiers, &launch.margin_tiers)?;
        }
        if notice.markup_bps.is_some_and(|markup| !(-MAX_RATE_BPS..=MAX_RATE_BPS).contains(&markup)) {
            return Err("financing.markup_bps must be within ±10000".into());
        }
        self.check_notice_time("a listing notice", notice.effective_at)?;
        self.operator_bounds.markups.insert(market, notice.clone());
        Ok(())
    }

    /// Checks a listing's spread bands are written as the version in force lists them, and that a
    /// relisting changes a listed market's markup (from `V49`) and its spread and margin bands (from
    /// `V52`) only to the values its notice names, once that notice's time has come. Returns what
    /// applying the listing spends.
    /// Error cases:
    /// - spread bands `check_spread_tiers` refuses
    /// - a changed markup, spread band table or margin band table no notice names, or whose notice
    ///   takes effect later than the sequenced time
    pub(crate) fn check_listing_change(&self, spec: &MarketSpec) -> Result<ListingChange, String> {
        check_spread_tiers(&spec.spread_tiers, spec.asset_class, self.protocol_version)?;
        let markup = self.check_markup_change(spec)?;
        let Some(market) = self.markets.get(&spec.id) else {
            return Ok(ListingChange { first: true, markup, ..ListingChange::default() });
        };
        let mut change = ListingChange { markup, ..ListingChange::default() };
        if self.protocol_version < ProtocolVersion::V52 {
            return Ok(change);
        }
        let notice = self.operator_bounds.markups.get(&spec.id);
        if spec.spread_tiers != market.spec.spread_tiers {
            let named = notice.filter(|notice| notice.spread_tiers.as_ref() == Some(&spec.spread_tiers));
            self.check_noticed(spec.id, "spread band", named)?;
            change.spread = true;
        }
        if spec.margin_tiers != market.spec.margin_tiers {
            let named = notice.filter(|notice| notice.margin_tiers.as_ref() == Some(&spec.margin_tiers));
            self.check_noticed(spec.id, "margin band", named)?;
            change.margin = true;
        }
        Ok(change)
    }

    /// After a listing applies: from `V52` a first listing's bands become the market's launch bands
    /// and its bound ceiling proposal is spent, and each noticed value it applied is spent, the
    /// notice with them once it names nothing more.
    pub(crate) fn settle_listing_change(&mut self, spec: &MarketSpec, change: ListingChange) {
        if change.first && self.protocol_version >= ProtocolVersion::V52 {
            let launch =
                LaunchBands { spread_tiers: spec.spread_tiers.clone(), margin_tiers: spec.margin_tiers.clone() };
            self.operator_bounds.launch_bands.insert(spec.id, launch);
            self.operator_bounds.proposed_listings.remove(&spec.id);
        }
        let Some(notice) = self.operator_bounds.markups.get_mut(&spec.id) else { return };
        if change.markup {
            notice.markup_bps = None;
        }
        if change.spread {
            notice.spread_tiers = None;
        }
        if change.margin {
            notice.margin_tiers = None;
        }
        if notice.names_nothing() {
            self.operator_bounds.markups.remove(&spec.id);
        }
    }

    /// Checks a changed `what` table is named by `notice` and its time has come.
    /// Error cases:
    /// - no notice names it, or the notice takes effect later than the sequenced time
    fn check_noticed(&self, market: MarketId, what: &str, notice: Option<&ListingNotice>) -> Result<(), String> {
        match notice {
            None => Err(format!("market {market}'s {what} change needs a notice of at least seven days")),
            Some(notice) if notice.effective_at > self.now => {
                Err(format!("market {market}'s {what} change takes effect from {}", notice.effective_at))
            }
            Some(_) => Ok(()),
        }
    }
}

/// The rate of the band that charges the notional just above `notional`. `tiers` is a validated
/// table, whose last band is unbounded.
fn rate_above(tiers: &[SpreadTier], notional: Fixed) -> i64 {
    tiers.iter().find(|tier| tier.up_to.is_none_or(|up_to| up_to > notional)).map_or(0, SpreadTier::tenth_bps)
}

/// Checks that at every notional `tiers` charges at least `launch`'s rate there and at most twice
/// it: both are constant between the bounds of either table, so each stretch is checked once.
/// Error cases:
/// - a stretch where the rate is below the launch rate or above twice it
fn check_spread_within_launch(tiers: &[SpreadTier], launch: &[SpreadTier]) -> Result<(), String> {
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

/// Checks that at every notional the margin `tiers` require is at least what `launch` requires. Both
/// requirements are linear between the bounds of either table, so they are compared at each bound,
/// and past the last by the last band's rate.
/// Error cases:
/// - a bound where the requirement, as the engine rounds it, is below launch's, or a last rate below
///   launch's
fn check_margin_at_or_above_launch(tiers: &[Tier], launch: &[Tier]) -> Result<(), String> {
    let required = |table: &[Tier], notional| staircase(table, Fixed::ZERO, notional, Rounding::Up);
    for bound in tiers.iter().chain(launch).filter_map(|tier| tier.up_to) {
        if required(tiers, bound) < required(launch, bound) {
            return Err(format!("the margin requirement at {bound} USD is below its launch requirement"));
        }
    }
    let last = |table: &[Tier]| table.last().map_or(0, |tier| tier.bps);
    if last(tiers) < last(launch) {
        return Err(format!("the margin rate past the last band is below its launch rate of {}bps", last(launch)));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
