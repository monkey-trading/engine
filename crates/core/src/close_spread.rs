//! Close-spread ceilings, from `V39`.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingCeiling {
    pub market: MarketId,
    pub max_close_spread_bps: i64,
    /// The sequenced time the adoption takes effect.
    pub effective_at: Millis,
}

/// The largest close charge `params` allow: the widest stress minimum spread plus the top toxicity
/// add-on, the sum a pure close pays at the most stressful admitted parameters.
pub(crate) fn launch_close_charge_bps(params: &ProtocolParams) -> i64 {
    let stress = params.stress_levels.iter().map(|level| level.min_spread_bps).max().unwrap_or(0);
    let toxicity = params.toxicity.tiers.iter().map(|tier| tier.extra_spread_bps).max().unwrap_or(0);
    stress + toxicity
}

impl Engine {
    /// The adopted close-spread ceilings by market, once genesis (or one signed install) records
    /// them; `None` until then.
    pub fn close_spread_ceilings(&self) -> Option<&BTreeMap<MarketId, i64>> {
        self.close_spread_ceilings.as_ref()
    }

    /// The ceiling adoption whose notice has run at `ts`, if one is pending.
    pub(crate) fn due_ceiling(&self, ts: Millis) -> Option<PendingCeiling> {
        self.pending_ceiling.filter(|pending| pending.effective_at <= ts)
    }

    /// Applies the pending adoption once its notice has run (see `Engine::due_ceiling`), before
    /// the command stamped `ts` acts.
    pub(crate) fn apply_due_ceiling(&mut self, ts: Millis) {
        let Some(pending) = self.due_ceiling(ts) else { return };
        // The registry outlives its install command: adoption needs it (see
        // `Engine::adopt_close_spread_ceiling`).
        self.close_spread_ceilings
            .get_or_insert_with(BTreeMap::new)
            .insert(pending.market, pending.max_close_spread_bps);
        self.pending_ceiling = None;
    }

    /// Installs the close-spread ceiling registry: one entry per listed market, adopted in a
    /// single command so the set is complete or refused. The registry binds from `V39`; it may be
    /// installed earlier, and a log upgrading to `V39` adopts it before activating. The node
    /// sequences it at genesis; a log records it once.
    /// Error cases:
    /// - once a registry is installed
    /// - before parameters are set
    /// - two entries for one market
    /// - a listed market with no entry
    /// - a value outside 0..=9999 bps, or a nonzero value below the parameters' largest close
    ///   charge, which would clamp a launch close by more than the rounding unit
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
    /// `V39`, like the registry itself.
    /// Error cases:
    /// - before the registry is installed
    /// - a value outside 0..=9999 bps, or not strictly below an adopted entry's
    /// - an effective time less than seven days after the sequenced time, or past the latest stamp
    pub(crate) fn adopt_close_spread_ceiling(
        &mut self,
        market: MarketId,
        max_close_spread_bps: i64,
        effective_at: Millis,
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
        self.pending_ceiling = Some(PendingCeiling { market, max_close_spread_bps, effective_at });
        Ok(())
    }

    /// Checks a listing against the ceiling registry, from `V39` once the registry is installed:
    /// a market with no entry cannot be listed, and an entry follows its market id, so a listed
    /// market's `asset_class` and `quote_currency` — its instrument's identity — cannot change.
    /// Error cases:
    /// - a listing of a market the registry has no entry for
    /// - a change of `asset_class` or `quote_currency` on a market with an entry
    pub(crate) fn check_listing_ceilings(&self, spec: &MarketSpec) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V39 {
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
        }
        Ok(())
    }

    /// Checks an activation carrying `version` past `V39`: every listed market must have an
    /// adopted ceiling, since from `V39` a close's spread is bounded by its market's entry.
    /// Error cases:
    /// - activating `V39` or later while a listed market has no adopted ceiling
    pub(crate) fn check_close_spread_activation(&self, version: ProtocolVersion) -> Result<(), String> {
        if version < ProtocolVersion::V39 {
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
                ProtocolVersion::V39
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
        if self.protocol_version < ProtocolVersion::V39 {
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
