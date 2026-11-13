//! Close-spread ceilings, from `V40`.
//!
//! The total spread a close can pay is capped per market: a registry maps each market to the most
//! its closing slice may ever pay, whatever an operator command does to the parameters, stress
//! levels, toxicity tiers, or spread tiers. The registry lives apart from `MarketSpec`, so
//! `UpsertMarket` can neither replace nor raise an entry. A pure close's charge clamps to the
//! ceiling, floored to the exact bound `charge_raw * 10_000 <= index_raw * ceiling_bps`; a
//! reversal whose blended spread — the stress and toxicity charge plus the opening slice's size
//! cost per unit — would exceed the ceiling is refused, while the pure close stays open; a
//! triggered order refused that way fills its closing slice and cancels the rest. After genesis, a
//! ceiling enters only by a signed adoption logged at least seven days before it takes effect —
//! the governance channel, with the notice the other operator bounds use — and an entry can only
//! be lowered, never raised.
//!
//! From `V52` closes pay the size spread, so a ceiling is at least its market's launch close charge,
//! which counts the first band at the widest size multiplier: an install or adoption below it, under
//! the parameters in force or those the market launched under, is refused, an adoption for a market
//! not yet listed binds the listing that market's first listing must be, and that first listing is
//! refused while its own launch close charge exceeds the ceiling. The activation raises a listed
//! market's ceiling, and a pending adoption's value, to its launch close charge. A pure close's
//! charge clamps to the ceiling before the check that a spread stays under the price, so a pure close
//! always prices.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::account::RestingOrder;
use crate::engine::Engine;
use crate::event::{CancelReason, Event, Reject};
use crate::execution::{Exec, PriceGuard};
use crate::market::MarketSpec;
use crate::operator_bounds::NOTICE_MS;
use crate::params::ProtocolParams;
use crate::protocol::ProtocolVersion;
use crate::spread_tier::check_spread_tiers;
use crate::types::{AccountId, MAX_STAMP_MS, MarketId, Millis, OrderId, Side};

/// The most a close-spread ceiling may be: 9,999 bps, an arithmetic limit, not an economic range.
pub const MAX_CLOSE_SPREAD_BPS: i64 = 9_999;

/// A market's ceiling on the total spread a close can pay, in bps of the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseSpreadCeiling {
    pub market: MarketId,
    pub max_close_spread_bps: i64,
}

/// A ceiling adoption waiting out its notice.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingCeiling {
    pub market: MarketId,
    pub max_close_spread_bps: i64,
    /// The sequenced time the adoption takes effect.
    pub effective_at: Millis,
    /// From `V52`, the listing an adoption for a market not yet listed binds; left out of the
    /// encoding when none is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listing: Option<MarketSpec>,
}

/// The largest close charge `params` allow: the widest stress minimum spread plus the top toxicity
/// add-on, the sum a pure close pays at the most stressful admitted parameters.
pub(crate) fn launch_close_charge_bps(params: &ProtocolParams) -> i64 {
    let stress = params.stress_levels.iter().map(|level| level.min_spread_bps).max().unwrap_or(0);
    let toxicity = params.toxicity.tiers.iter().map(|tier| tier.extra_spread_bps).max().unwrap_or(0);
    stress + toxicity
}

/// From `V52`, the largest charge a close in its first band can pay in `spec`'s market under
/// `params`, in tenths of a bp: the widest stress minimum and the top toxicity add-on, plus the
/// first spread band at the widest size multiplier, rounded up.
pub(crate) fn launch_close_charge_tenth_bps(params: &ProtocolParams, spec: &MarketSpec) -> i64 {
    let multiplier = params.stress_levels.iter().map(|level| level.size_spread_multiplier_bps).max().unwrap_or(0);
    let first = spec.spread_tiers.first().map_or(0, |tier| tier.tenth_bps());
    // Validated rates (0 to 100,000 tenths, and a 0 to 100,000 bps multiplier) keep this inside i64
    // and non-negative, so adding the divisor less one rounds up.
    launch_close_charge_bps(params) * 10 + (first * multiplier + 9_999) / 10_000
}

impl Engine {
    /// Checks a ceiling covers `spec`'s launch close charge under `params`, and the one its market
    /// launched under, once it has.
    /// Error cases:
    /// - a ceiling below either launch close charge, in tenths of a bp
    fn check_ceiling_floor(&self, params: &ProtocolParams, spec: &MarketSpec, ceiling_bps: i64) -> Result<(), String> {
        let launched =
            self.operator_bounds.launch_bands.get(&spec.id).map_or(0, |launch| launch.close_charge_tenth_bps);
        let floor = launch_close_charge_tenth_bps(params, spec).max(launched);
        if ceiling_bps * 10 < floor {
            return Err(format!(
                "market {}'s close-spread ceiling of {ceiling_bps}bps is below its launch close charge of {}.{}bps",
                spec.id,
                floor / 10,
                floor % 10
            ));
        }
        Ok(())
    }

    /// On `V52`'s activation, once launch bands are recorded: raises each listed market's ceiling,
    /// and a pending adoption's value for one, to its launch close charge rounded up to a whole bp,
    /// so no ceiling set before closes paid the size spread waives it.
    pub(crate) fn raise_ceilings_to_launch(&mut self) {
        let floor = |launch: &crate::listing_bounds::LaunchBands| {
            ((launch.close_charge_tenth_bps + 9) / 10).min(MAX_CLOSE_SPREAD_BPS)
        };
        let launch_bands = &self.operator_bounds.launch_bands;
        for (market, ceiling) in self.close_spread_ceilings.iter_mut().flatten() {
            if let Some(launch) = launch_bands.get(market) {
                *ceiling = (*ceiling).max(floor(launch));
            }
        }
        if let Some(pending) = self.pending_ceiling.as_mut()
            && let Some(launch) = launch_bands.get(&pending.market)
        {
            pending.max_close_spread_bps = pending.max_close_spread_bps.max(floor(launch));
        }
    }

    /// The adopted close-spread ceilings by market, once genesis (or one signed install) records
    /// them; `None` until then.
    pub fn close_spread_ceilings(&self) -> Option<&BTreeMap<MarketId, i64>> {
        self.close_spread_ceilings.as_ref()
    }

    /// Applies the pending adoption once its notice has run, before the command stamped `ts` acts,
    /// with the listing it binds, if it names one.
    pub(crate) fn apply_due_ceiling(&mut self, ts: Millis) {
        let Some(pending) = self.pending_ceiling.take_if(|pending| pending.effective_at <= ts) else { return };
        // The registry outlives its install command: adoption needs it (see
        // `Engine::adopt_close_spread_ceiling`).
        self.close_spread_ceilings
            .get_or_insert_with(BTreeMap::new)
            .insert(pending.market, pending.max_close_spread_bps);
        // A market listed while the adoption waited was held to its listing already.
        if let Some(listing) = pending.listing.filter(|_| !self.markets.contains_key(&pending.market)) {
            self.operator_bounds.proposed_listings.insert(pending.market, listing);
        }
    }

    /// Installs the close-spread ceiling registry: one entry per listed market, adopted in a
    /// single command so the set is complete or refused. The registry binds from `V40`; it may be
    /// installed earlier, and a log upgrading to `V40` adopts it before activating. The node
    /// sequences it at genesis; a log records it once.
    /// Error cases:
    /// - once a registry is installed
    /// - before parameters are set
    /// - two entries for one market
    /// - a listed market with no entry
    /// - a value outside 0..=9999 bps, or a nonzero value below the parameters' largest close
    ///   charge, which would clamp a launch close by more than the rounding unit
    /// - from `V52`, a value below its listed market's launch close charge under the parameters in
    ///   force or those it launched under, zero included
    pub(crate) fn install_close_spread_ceilings(&mut self, ceilings: &[CloseSpreadCeiling]) -> Result<(), String> {
        if self.close_spread_ceilings.is_some() {
            return Err("the close-spread ceiling registry is already installed".into());
        }
        let params = self.params.as_ref().ok_or("protocol params not set")?;
        let floor = launch_close_charge_bps(params);
        let mut registry = BTreeMap::new();
        for ceiling in ceilings {
            if registry.insert(ceiling.market, ceiling.max_close_spread_bps).is_some() {
                return Err(format!("market {} takes a close-spread ceiling twice", ceiling.market));
            }
            if !(0..=MAX_CLOSE_SPREAD_BPS).contains(&ceiling.max_close_spread_bps) {
                return Err(format!("max_close_spread_bps must be in 0..={MAX_CLOSE_SPREAD_BPS}"));
            }
            if ceiling.max_close_spread_bps != 0 && ceiling.max_close_spread_bps < floor {
                return Err(format!(
                    "a nonzero ceiling below the {floor}bps largest close charge would clamp a launch close"
                ));
            }
            if let Some(market) = self.markets.get(&ceiling.market)
                && self.protocol_version >= ProtocolVersion::V52
            {
                self.check_ceiling_floor(params, &market.spec, ceiling.max_close_spread_bps)?;
            }
        }

        if let Some(id) = self.markets.keys().find(|id| !registry.contains_key(*id)) {
            return Err(format!("market {id} is listed without a close-spread ceiling"));
        }
        self.close_spread_ceilings = Some(registry);
        Ok(())
    }

    /// Records a signed adoption of one close-spread ceiling, replacing any pending adoption: an
    /// entry for a market the registry has none for — a market not yet listed — or a strictly
    /// lower value for an adopted one, in effect from `effective_at`. The adoption binds from
    /// `V40`, like the registry itself. From `V52` an adoption for a market not yet listed names
    /// the `listing` its first listing must be.
    /// Error cases:
    /// - before the registry is installed
    /// - a value outside 0..=9999 bps, or not strictly below an adopted entry's
    /// - an effective time less than seven days after the sequenced time, or past the latest stamp
    /// - a listing before `V52`, or from `V52` one for a listed market, none for a market not yet
    ///   listed, or one that is for another market, fails `MarketSpec::validate` or
    ///   `check_spread_tiers`
    /// - from `V52`, a value below the launch close charge of the listed market under the parameters
    ///   in force or those it launched under, or of the listing
    pub(crate) fn adopt_close_spread_ceiling(
        &mut self,
        market: MarketId,
        max_close_spread_bps: i64,
        effective_at: Millis,
        listing: Option<&MarketSpec>,
    ) -> Result<(), String> {
        let ceilings =
            self.close_spread_ceilings.as_ref().ok_or("the close-spread ceiling registry is not installed")?;
        if !(0..=MAX_CLOSE_SPREAD_BPS).contains(&max_close_spread_bps) {
            return Err(format!("max_close_spread_bps must be in 0..={MAX_CLOSE_SPREAD_BPS}"));
        }
        if let Some(&current) = ceilings.get(&market)
            && max_close_spread_bps >= current
        {
            return Err(format!("market {market}'s close-spread ceiling can only be lowered from {current}bps"));
        }
        if effective_at < self.now + NOTICE_MS {
            return Err(format!("a ceiling adoption takes effect no sooner than {}", self.now + NOTICE_MS));
        }
        if effective_at > MAX_STAMP_MS {
            return Err(format!("a ceiling adoption takes effect no later than {MAX_STAMP_MS}"));
        }
        self.check_adopted_floor(market, max_close_spread_bps, listing)?;
        let listing = listing.cloned();
        self.pending_ceiling = Some(PendingCeiling { market, max_close_spread_bps, effective_at, listing });
        Ok(())
    }

    /// Checks an adoption's listing and floor: none before `V52`, and from `V52` the ceiling covers
    /// the launch close charge of the listed market, or of the listing an adoption for a market not
    /// yet listed names.
    /// Error cases:
    /// - see `Engine::adopt_close_spread_ceiling`
    fn check_adopted_floor(&self, market: MarketId, ceiling: i64, listing: Option<&MarketSpec>) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V52 {
            return match listing {
                Some(_) => Err(format!("an adoption's listing needs protocol {:?}", ProtocolVersion::V52)),
                None => Ok(()),
            };
        }
        // The registry is installed only once parameters are set.
        let params = self.params.as_ref().expect("an installed registry has parameters");
        let spec = match (self.markets.get(&market), listing) {
            (Some(_), Some(_)) => return Err(format!("market {market} is listed, so its adoption names no listing")),
            (None, None) => {
                return Err(format!("market {market} is not listed, so its adoption names the listing it binds"));
            }
            (Some(listed), None) => &listed.spec,
            (None, Some(spec)) => {
                if spec.id != market {
                    return Err(format!("an adoption for market {market} names a listing of market {}", spec.id));
                }
                spec.validate()?;
                check_spread_tiers(&spec.spread_tiers, spec.asset_class, self.protocol_version)?;
                spec
            }
        };
        self.check_ceiling_floor(params, spec, ceiling)
    }

    /// Checks a listing against the ceiling registry, from `V40` once the registry is installed:
    /// a market with no entry cannot be listed, and an entry follows its market id, so a listed
    /// market's `asset_class` and `quote_currency` — its instrument's identity — cannot change. From
    /// `V52` a market's first listing is the one its adoption bound, if one did, and its launch close
    /// charge is within its ceiling.
    /// Error cases:
    /// - a listing of a market the registry has no entry for
    /// - a change of `asset_class` or `quote_currency` on a market with an entry
    /// - from `V52`, a first listing other than the one an applied or pending adoption bound, or
    ///   whose launch close charge under the parameters in force exceeds its ceiling or a pending
    ///   adoption's lower value
    pub(crate) fn check_listing_ceilings(&self, spec: &MarketSpec) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V40 {
            return Ok(());
        }
        let Some(ceilings) = &self.close_spread_ceilings else { return Ok(()) };
        // An entry implies a listed market: the install that adopts entries validates each one
        // against the markets listed then, and an adoption for a not-yet-listed market only lands
        // where no entry exists.
        if !ceilings.contains_key(&spec.id) {
            return Err(format!("market {} has no adopted close-spread ceiling, so it cannot be listed", spec.id));
        }
        // An entry follows its market id: a relisting cannot turn it into a different instrument.
        if let Some(market) = self.markets.get(&spec.id) {
            if market.spec.asset_class != spec.asset_class {
                return Err(format!("market {} has a ceiling, so its asset class cannot change", spec.id));
            }
            if market.spec.quote_currency != spec.quote_currency {
                return Err(format!("market {} has a ceiling, so its quote currency cannot change", spec.id));
            }
        } else if self.protocol_version >= ProtocolVersion::V52 {
            // An adoption still waiting out its notice binds the listing too, and its lower value is
            // the one the listing will close under.
            let pending = self.pending_ceiling.as_ref().filter(|pending| pending.market == spec.id);
            let proposals =
                [self.operator_bounds.proposed_listings.get(&spec.id), pending.and_then(|p| p.listing.as_ref())];
            if proposals.into_iter().flatten().any(|proposal| proposal != spec) {
                return Err(format!(
                    "market {}'s first listing must be the listing its ceiling adoption bound",
                    spec.id
                ));
            }
            // The registry is installed only once parameters are set.
            let params = self.params.as_ref().expect("an installed registry has parameters");
            let ceiling = pending.map_or(ceilings[&spec.id], |pending| pending.max_close_spread_bps);
            self.check_ceiling_floor(params, spec, ceiling.min(ceilings[&spec.id]))?;
        }
        Ok(())
    }

    /// Checks an activation carrying `version` past `V40`: every listed market must have an
    /// adopted ceiling, since from `V40` a close's spread is bounded by its market's entry.
    /// Error cases:
    /// - activating `V40` or later while a listed market has no adopted ceiling
    pub(crate) fn check_close_spread_activation(&self, version: ProtocolVersion) -> Result<(), String> {
        if version < ProtocolVersion::V40 {
            return Ok(());
        }
        let missing: Vec<_> = self
            .markets
            .keys()
            .filter(|id| !self.close_spread_ceilings.as_ref().is_some_and(|ceilings| ceilings.contains_key(*id)))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "markets {missing:?} are listed without a close-spread ceiling; adopt one for each before protocol {:?}",
                ProtocolVersion::V40
            ));
        }
        Ok(())
    }

    /// The market's ceiling on the total spread a close can pay, once the registry holds one.
    pub(crate) fn close_spread_ceiling(&self, market: MarketId) -> Option<i64> {
        self.close_spread_ceilings.as_ref()?.get(&market).copied()
    }

    /// Bounds the per-unit charge by the market's close-spread ceiling. A trade with no closing
    /// slice — an opening or a same-side add — pays what it priced. A pure close's charge clamps
    /// to the ceiling, floored to the exact bound `charge_raw * 10_000 <= index_raw *
    /// ceiling_bps`, trimming at most the unit upward rounding adds. A reversal pays the blended
    /// charge on its closing slice, so it is refused when that exceeds the ceiling; one that
    /// passes pays what it priced, which the check keeps within the ceiling.
    /// Error cases:
    /// - a reversal whose blended charge exceeds the market's ceiling
    pub(crate) fn bounded_close_charge(
        &self,
        market: MarketId,
        side: Side,
        pos_qty: Fixed,
        increases: bool,
        charge: Fixed,
        mid: Fixed,
    ) -> Result<Fixed, Reject> {
        if self.protocol_version < ProtocolVersion::V40 {
            return Ok(charge);
        }
        let Some(ceiling) = self.close_spread_ceiling(market) else { return Ok(charge) };
        // The trade closes only when its side opposes the position it meets.
        let closes = (side == Side::Buy) != pos_qty.is_positive() && !pos_qty.is_zero();
        if !closes {
            return Ok(charge);
        }
        // Index mids are bounded by `Fixed::MAX_INPUT`, so these products stay far inside i128.
        if increases {
            if charge.raw() * 10_000 > mid.raw() * i128::from(ceiling) {
                return Err(Reject::CloseSpreadCeiling { ceiling_bps: ceiling });
            }
            return Ok(charge);
        }
        let bound = mid.raw() * i128::from(ceiling) / 10_000;
        Ok(charge.min(Fixed::from_raw(bound)))
    }

    /// Fills the closing slice of a triggered order a reversal refusal stopped, then cancels the
    /// order: a triggered order has no client at hand to send the reduce-only close the refusal
    /// asks for. The closing slice is a pure close, so it fills at its clamped charge; if even it
    /// cannot fill, the order cancels as unfillable and the position stays open.
    pub(crate) fn fill_closing_slice(
        &mut self,
        account: AccountId,
        order_id: OrderId,
        order: &RestingOrder,
        guard: PriceGuard,
        events: &mut Vec<Event>,
    ) {
        let pos_qty = self.accounts.get(&account).map(|acct| acct.position(order.market).qty.abs()).unwrap_or_default();
        let fill = if pos_qty.is_zero() {
            None
        } else {
            let exec = Exec {
                account,
                order_id,
                market: order.market,
                side: order.side,
                qty: pos_qty.min(order.qty),
                reduce_only: true,
                guard,
                liquidation: false,
            };
            self.execute(exec, events).ok()
        };
        self.remove_order(account, order_id);
        if let Some(fill) = fill {
            self.after_fill(fill, events);
        }
        events.push(Event::OrderCancelled { account, order_id, reason: CancelReason::Unfillable });
    }
}
