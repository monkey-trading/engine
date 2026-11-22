//! Listing bounds, from `V52`: a listed market's spread and margin bands move only as a listing
//! notice of at least seven days names them, and only within bounds its launch bands set. From `V60`
//! so does its weekend margin (`weekend`), to none or to a rate the listing's validation allows, and
//! from `V79` its minimum level distance (`levels`), from its launch value to twice it.
//!
//! A notice may name a market's spread bands, margin bands and financing markup (`V49`'s markup
//! notice is the notice naming only a markup), and a relisting applies each named value once, from
//! the notice's time on. At every notional a noticed spread rate is at least the launch rate there
//! and at most twice it, and a noticed margin requirement, unrounded, at least launch's. The launch
//! bands are the listing in force when `V52` activates, each spread rate raised to its class's floor,
//! or a market's first listing after it; with them is the market's launch close charge under the
//! parameters in force then, below which no ceiling is adopted (see `close_spread`). A new listing
//! needs no notice.
//!
//! From `V76` a notice may name a market's price rules, and price rules that loosen any of the rules in
//! force, fewer sources, a longer freshness bound or a wider divergence bound, apply only as a notice
//! or a vote names them, and never looser in any rule than the market's launch rules: those in force
//! when `V76` activates, or its first listing after. Tighter price rules apply at once before `V87`.
//!
//! From `V82` a relisting's calendar shuts a listed market only from seven days on, unless a vote carries
//! it: the new calendar has the market open at every instant before then that the calendar in force does,
//! whatever its weekend margin and whether or not it holds positions, since one opened before the closure
//! starts would meet it with no notice. A first listing takes any calendar.
//!
//! From `V87` tighter price rules apply only as a notice or a vote names them too, and so do a futures
//! market's contracts from the one it prices from on: each stays at its name and roll time, in order, but
//! for contracts added after the last. Either holds whether or not the market is reduce-only or holds
//! positions, so no relisting leaves a market's quotes unable to price it, or its roll without a next
//! contract the venues quote, at once.

use serde::{Deserialize, Serialize};

use crate::close_spread::launch_close_charge_tenth_bps;
use crate::engine::Engine;
use crate::futures::FuturesContract;
use crate::levels::check_level_distance_range;
use crate::market::{MarketSpec, PriceRules, Tier, validate_tiers};
use crate::operator_bounds::NOTICE_MS;
use crate::params::MAX_RATE_BPS;
use crate::protocol::ProtocolVersion;
use crate::session::Schedule;
use crate::spread_tier::{SpreadTier, check_spread_tiers, validate_spread_tiers};
use crate::types::{MarketId, Millis};

mod bands;
use crate::listing_bounds::bands::check_margin_at_or_above_launch;
pub(crate) use crate::listing_bounds::bands::check_spread_within_launch;

/// The operator's notice of one listed market's new listing values: from `V49` its financing markup,
/// from `V52` its spread and margin bands, from `V60` its weekend margin, from `V76` its price rules,
/// from `V79` its minimum level distance, and from `V87` its futures contracts. A relisting may apply
/// each named value from `effective_at` on, once. A notice naming only a markup encodes as `V49`'s did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListingNotice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spread_tiers: Option<Vec<SpreadTier>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin_tiers: Option<Vec<Tier>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markup_bps: Option<i64>,
    /// From `V60`, a weekend margin rate, zero for none, left out of the encoding when none is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekend_margin_bps: Option<i64>,
    /// From `V76`, price rules, left out of the encoding when none are named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_rules: Option<PriceRules>,
    /// From `V79`, a minimum level distance, left out of the encoding when none is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_level_distance_bps: Option<i64>,
    /// From `V87`, a futures market's contracts, in roll order, left out of the encoding when none are named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contracts: Option<Vec<FuturesContract>>,
    /// The sequenced time the named values may apply from.
    pub effective_at: Millis,
}

/// A market's launch bands, which bound the bands a listing notice may name, and its launch close
/// charge, which bounds the ceilings it may adopt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchBands {
    pub spread_tiers: Vec<SpreadTier>,
    pub margin_tiers: Vec<Tier>,
    /// The launch close charge, in tenths of a bp, under the parameters in force at launch.
    pub close_charge_tenth_bps: i64,
    /// From `V76`, the launch price rules, past which no price rules loosen; left out of the encoding
    /// while none are recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_rules: Option<PriceRules>,
    /// From `V79`, the minimum level distance in force at activation or at the market's first listing
    /// after it, which bounds the distances a notice or a vote may set. Left out of the encoding while
    /// unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_level_distance_bps: Option<i64>,
}

/// What a listing changes that its market's notice names, so applying it spends those values, and
/// whether it is the market's first listing.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ListingChange {
    first: bool,
    spread: bool,
    margin: bool,
    markup: bool,
    weekend: bool,
    price_rules: bool,
    distance: bool,
    contracts: bool,
}

impl ListingNotice {
    /// A notice of `markup_bps` alone, as `AnnounceMarkup` gives.
    pub fn markup(markup_bps: i64, effective_at: Millis) -> Self {
        ListingNotice {
            spread_tiers: None,
            margin_tiers: None,
            markup_bps: Some(markup_bps),
            weekend_margin_bps: None,
            price_rules: None,
            min_level_distance_bps: None,
            contracts: None,
            effective_at,
        }
    }

    /// Whether the notice names no value left to apply.
    fn names_nothing(&self) -> bool {
        self.spread_tiers.is_none()
            && self.margin_tiers.is_none()
            && self.markup_bps.is_none()
            && self.weekend_margin_bps.is_none()
            && self.price_rules.is_none()
            && self.min_level_distance_bps.is_none()
            && self.contracts.is_none()
    }
}

impl Engine {
    /// On an activation carrying `version` to `V52` or later from before it: raises each stress
    /// level's size multiplier to at least 1x, writes every listed market's spread bands in tenths of
    /// a bp, each raised to its class's floor, and takes the result, the margin bands and the launch
    /// close charge as the market's launch bands, which `Engine::check_close_spread_activation` has
    /// checked every ceiling covers.
    pub(crate) fn start_listing_bounds(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V52 || version < ProtocolVersion::V52 {
            return;
        }
        if let Some(params) = self.params.as_mut() {
            params.raise_size_multipliers();
        }
        let ids: Vec<MarketId> = self.markets.keys().copied().collect();
        for id in ids {
            let spec = &mut self.markets.get_mut(&id).expect("listed").spec;
            spec.convert_spread_tiers();
            let launch = self.launch_of(&self.markets[&id].spec);
            self.operator_bounds.launch_bands.insert(id, launch);
        }
    }

    /// On an activation carrying `version` to `V76` or later from before it, takes each listed market's
    /// price rules as its launch rules.
    pub(crate) fn start_price_rule_bounds(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V76 || version < ProtocolVersion::V76 {
            return;
        }
        for (id, market) in self.markets.iter() {
            let launch = self
                .operator_bounds
                .launch_bands
                .get_mut(id)
                .expect("V52's activation records every market listed then, and each first listing after it");
            launch.price_rules = Some(market.spec.price_rules);
        }
    }

    /// `spec` as its market's launch bands, with its launch close charge under the parameters in
    /// force, none before parameters are set, from `V76` its price rules, and from `V79` its minimum level
    /// distance.
    pub(crate) fn launch_of(&self, spec: &MarketSpec) -> LaunchBands {
        LaunchBands {
            price_rules: (self.protocol_version >= ProtocolVersion::V76).then_some(spec.price_rules),
            spread_tiers: spec.spread_tiers.clone(),
            margin_tiers: spec.margin_tiers.clone(),
            close_charge_tenth_bps: self
                .params
                .as_ref()
                .map_or(0, |params| launch_close_charge_tenth_bps(params, spec)),
            min_level_distance_bps: spec
                .min_level_distance_bps
                .filter(|_| self.protocol_version >= ProtocolVersion::V79),
        }
    }

    /// From `V52`, records a notice of `market`'s new listing values, replacing any pending one for
    /// that market.
    /// Error cases:
    /// - before `V52`, or for a market that is not listed
    /// - a notice naming nothing
    /// - spread bands that fail `MarketSpec::validate`'s table checks or `check_spread_tiers`, or
    ///   whose rate at some notional is below the launch rate there or above twice it, or from `V59`
    ///   that `Engine::check_tiered_listing` refuses
    /// - margin bands that fail the table checks, or whose requirement at some notional is below
    ///   launch's
    /// - a markup outside ±10000 bps a year
    /// - a weekend margin before `V60`, or one the listing, with any margin bands the notice names,
    ///   would fail `MarketSpec::validate` with
    /// - price rules before `V76`, ones the listing would fail `MarketSpec::validate` with, or ones
    ///   looser in any rule than the market's launch rules
    /// - a minimum level distance before `V79`, or one outside its launch value to twice it
    /// - futures contracts before `V87`, none, ones the listing would fail `MarketSpec::validate` with, or any
    ///   for a market not priced from futures
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
            self.check_tiered_listing(market, tiers)?;
        }
        if let Some(tiers) = &notice.margin_tiers {
            validate_tiers("margin_tiers", tiers, 1)?;
            check_margin_at_or_above_launch(tiers, &launch.margin_tiers)?;
        }
        if notice.markup_bps.is_some_and(|markup| !(-MAX_RATE_BPS..=MAX_RATE_BPS).contains(&markup)) {
            return Err("financing.markup_bps must be within ±10000".into());
        }
        if let Some(weekend_margin_bps) = notice.weekend_margin_bps {
            if self.protocol_version < ProtocolVersion::V60 {
                return Err(format!("a weekend margin notice needs protocol {:?}", ProtocolVersion::V60));
            }
            let margin_tiers = notice.margin_tiers.clone().unwrap_or_else(|| listed.spec.margin_tiers.clone());
            MarketSpec { weekend_margin_bps, margin_tiers, ..listed.spec.clone() }.validate()?;
        }
        if let Some(price_rules) = notice.price_rules {
            if self.protocol_version < ProtocolVersion::V76 {
                return Err(format!("a price rule notice needs protocol {:?}", ProtocolVersion::V76));
            }
            MarketSpec { price_rules, ..listed.spec.clone() }.validate()?;
            check_price_rules_within_launch(market, &price_rules, launch)?;
        }
        if let Some(bps) = notice.min_level_distance_bps {
            if self.protocol_version < ProtocolVersion::V79 {
                return Err(format!("a minimum level distance notice needs protocol {:?}", ProtocolVersion::V79));
            }
            check_level_distance_range(bps, launch.min_level_distance_bps)?;
        }
        if let Some(contracts) = &notice.contracts {
            if self.protocol_version < ProtocolVersion::V87 {
                return Err(format!("a futures contract notice needs protocol {:?}", ProtocolVersion::V87));
            }
            if listed.futures.is_none() {
                return Err(format!("market {market} is not priced from futures contracts"));
            }
            if contracts.is_empty() {
                return Err("a futures contract notice names at least one contract".into());
            }
            MarketSpec { contracts: contracts.clone(), ..listed.spec.clone() }.validate()?;
        }
        self.check_notice_time("a listing notice", notice.effective_at)?;
        self.operator_bounds.markups.insert(market, notice.clone());
        Ok(())
    }

    /// Checks a listing's spread bands are written as the version in force lists them, and that a
    /// relisting changes a listed market's markup (from `V49`), its spread and margin bands (from
    /// `V52`), its weekend margin (from `V60`), price rules that loosen any rule (from `V76`), or from
    /// `V87` that change at all, its minimum level distance (from `V79`) and, from `V87`, a futures
    /// market's contracts other than by adding after the last (`MarketState::keeps_contracts`) only to
    /// the values its notice names, once that notice's time has come, and from `V82` shuts the market
    /// only from seven days on (`check_calendar_notice`). A `voted` relisting's notice is its vote's,
    /// and it is held to the bounds a notice is: each changed band table within its market's launch
    /// bands, a weekend margin `MarketSpec::validate` admits, loosened price rules within the launch
    /// rules, and a distance from its launch value to twice it. Changes are measured from the baseline
    /// (`Engine::baseline_spec`).
    /// Returns what applying the listing spends.
    /// Error cases:
    /// - spread bands `check_spread_tiers` refuses
    /// - from `V59`, changed spread bands `Engine::check_tiered_listing` refuses, or a first listing's
    ///   bands `Engine::check_tiered_first_listing` refuses
    /// - from `V79`, a listed market's minimum level distance left out
    /// - unless `voted`, a changed markup, spread band table, margin band table or weekend margin, from
    ///   `V76` price rules looser in any rule, from `V79` a changed distance, or from `V87` tighter price
    ///   rules or changed futures contracts, from `V88` an added contract after the last to a market that
    ///   holds positions, no notice names, or whose notice takes effect later than the sequenced time or from
    ///   `V88` has lapsed
    /// - if `voted`, a changed spread band table whose rate at some notional is below the launch rate
    ///   there or above twice it, a changed margin band table whose requirement at some notional is
    ///   below launch's, from `V76` loosened price rules looser in any rule than the launch rules, or
    ///   from `V79` a distance outside its launch value to twice it
    /// - from `V82`, unless `voted`, a calendar that shuts a listed market within seven days at an
    ///   instant its calendar in force has it open
    pub(crate) fn check_listing_change(&self, spec: &MarketSpec, voted: bool) -> Result<ListingChange, String> {
        check_spread_tiers(&spec.spread_tiers, spec.asset_class, self.protocol_version)?;
        let markup = self.check_markup_change(spec, voted)?;
        let Some(listed) = self.baseline_spec(spec.id) else {
            self.check_tiered_first_listing(spec.id, &spec.spread_tiers)?;
            return Ok(ListingChange { first: true, markup, ..ListingChange::default() });
        };
        let mut change = ListingChange { markup, ..ListingChange::default() };
        if self.protocol_version < ProtocolVersion::V52 {
            return Ok(change);
        }
        let launch = self
            .operator_bounds
            .launch_bands
            .get(&spec.id)
            .expect("V52's activation records every market listed then, and each first listing after it");
        let notice = self.operator_bounds.markups.get(&spec.id);
        if spec.spread_tiers != listed.spread_tiers {
            self.check_tiered_listing(spec.id, &spec.spread_tiers)?;
            if voted {
                check_spread_within_launch(&spec.spread_tiers, &launch.spread_tiers)?;
            } else {
                let named = notice.filter(|notice| notice.spread_tiers.as_ref() == Some(&spec.spread_tiers));
                self.check_noticed(spec.id, "spread band", named)?;
                change.spread = true;
            }
        }
        if spec.margin_tiers != listed.margin_tiers {
            if voted {
                check_margin_at_or_above_launch(&spec.margin_tiers, &launch.margin_tiers)?;
            } else {
                let named = notice.filter(|notice| notice.margin_tiers.as_ref() == Some(&spec.margin_tiers));
                self.check_noticed(spec.id, "margin band", named)?;
                change.margin = true;
            }
        }
        if spec.weekend_margin_bps != listed.weekend_margin_bps && !voted {
            let named = notice.filter(|notice| notice.weekend_margin_bps == Some(spec.weekend_margin_bps));
            self.check_noticed(spec.id, "weekend margin", named)?;
            change.weekend = true;
        }
        let loosened = spec.price_rules.looser_than(&listed.price_rules);
        // From `V87` tighter rules need the notice too.
        let changed = self.protocol_version >= ProtocolVersion::V87 && spec.price_rules != listed.price_rules;
        if self.protocol_version >= ProtocolVersion::V76 && (loosened || changed) {
            if voted {
                check_price_rules_within_launch(spec.id, &spec.price_rules, launch)?;
            } else {
                let named = notice.filter(|notice| notice.price_rules == Some(spec.price_rules));
                self.check_noticed(spec.id, "price rule", named)?;
                change.price_rules = true;
            }
        }
        if self.protocol_version >= ProtocolVersion::V79 && spec.min_level_distance_bps != listed.min_level_distance_bps
        {
            let bps = spec.min_level_distance_bps.ok_or("a listed market keeps a minimum level distance")?;
            if voted {
                check_level_distance_range(bps, launch.min_level_distance_bps)?;
            } else {
                let named = notice.filter(|notice| notice.min_level_distance_bps == Some(bps));
                self.check_noticed(spec.id, "minimum level distance", named)?;
                change.distance = true;
            }
        }
        // From `V88` so does adding a contract after the last to a market that holds positions: one no venue
        // quotes would be the contract it must roll onto.
        let market = &self.markets[&spec.id];
        let adds_held = self.protocol_version >= ProtocolVersion::V88
            && self.holds_positions(spec.id)
            && market.adds_contracts(spec);
        if self.protocol_version >= ProtocolVersion::V87 && !voted && (!market.keeps_contracts(spec) || adds_held) {
            let named = notice.filter(|notice| notice.contracts.as_ref() == Some(&spec.contracts));
            self.check_noticed(spec.id, "futures contract", named)?;
            change.contracts = true;
        }
        if self.protocol_version >= ProtocolVersion::V82 && !voted {
            check_calendar_notice(spec.id, &listed.schedule, &spec.schedule, self.now)?;
        }
        Ok(change)
    }

    /// After a listing applies: from `V52` a first listing's bands become the market's launch bands
    /// and its bound ceiling proposal is spent, and each noticed value it applied is spent, the
    /// notice with them once it names nothing more.
    pub(crate) fn settle_listing_change(&mut self, spec: &MarketSpec, change: ListingChange) {
        if change.first && self.protocol_version >= ProtocolVersion::V52 {
            let launch = self.launch_of(spec);
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
        if change.weekend {
            notice.weekend_margin_bps = None;
        }
        if change.price_rules {
            notice.price_rules = None;
        }
        if change.distance {
            notice.min_level_distance_bps = None;
        }
        if change.contracts {
            notice.contracts = None;
        }
        if notice.names_nothing() {
            self.operator_bounds.markups.remove(&spec.id);
        }
    }

    /// Checks a changed `what` table is named by `notice` and its time has come, and from `V88` has not
    /// passed by more than `NOTICE_USE_MS` (`Engine::check_notice_in_force`).
    /// Error cases:
    /// - no notice names it, or the notice takes effect later than the sequenced time, or has lapsed
    fn check_noticed(&self, market: MarketId, what: &str, notice: Option<&ListingNotice>) -> Result<(), String> {
        match notice {
            None => Err(format!("market {market}'s {what} change needs a notice of at least seven days")),
            Some(notice) => {
                self.check_notice_in_force(notice.effective_at, &format!("market {market}'s {what} change"))
            }
        }
    }
}

/// Checks `schedule` shuts `market` only from `NOTICE_MS` after `now` on: at every instant before then
/// that `in_force` has the market open, `schedule` has it open too. A shut market's quotes set no mark
/// and its close-outs wait, so a closure sooner than that would freeze both with no notice. Hours added,
/// and hours shut only from `NOTICE_MS` on, pass.
/// Error cases:
/// - an instant within `NOTICE_MS` of `now` that `in_force` has the market open and `schedule` shut
fn check_calendar_notice(
    market: MarketId,
    in_force: &Schedule,
    schedule: &Schedule,
    now: Millis,
) -> Result<(), String> {
    if in_force == schedule {
        return Ok(());
    }
    match in_force.open_hours_shut_by(schedule, now, now + NOTICE_MS) {
        None => Ok(()),
        Some((open, close)) => Err(format!(
            "a calendar change may not shut market {market} within {} days in hours its calendar has it open, from {open} to {close}",
            NOTICE_MS / 86_400_000
        )),
    }
}

/// Checks `price_rules` are no looser in any rule than `market`'s launch rules.
/// Error cases:
/// - fewer sources, a longer freshness bound or a wider divergence bound than the launch rules
fn check_price_rules_within_launch(
    market: MarketId,
    price_rules: &PriceRules,
    launch: &LaunchBands,
) -> Result<(), String> {
    // `V76`'s activation records every listed market's rules, and each first listing after it.
    let launched = launch.price_rules.expect("launch price rules are recorded wherever V76 applies");
    if price_rules.looser_than(&launched) {
        return Err(format!(
            "market {market}'s price rules {price_rules:?} are looser than its launch rules {launched:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
