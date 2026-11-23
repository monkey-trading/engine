//! Bounds on the operator's powers, from `V35`.
//!
//! The profit fee and the close-out level move only within ±50% of their launch values and only
//! after a notice: a signed `AnnounceParams` names the new values and the time they apply from, at
//! least seven days after it is logged, and a `SetParams` that carries one applies only from then.
//! A price-rule change that would leave a market with open positions without a price is refused
//! unless the market is reduce-only. The operator key rotates only by a signed `RotateOperator`,
//! which takes effect seven days after it is logged; from then the old key signs nothing.
//!
//! From `V49` the stress parameters and a listed market's financing markup take the same seven-day
//! notice, with no range: an `AnnounceParams` may name new stress parameters, and an
//! `AnnounceMarkup` one market's new markup, each applying once from its time on. From `V52` that
//! markup notice is a listing notice, which may also name the market's spread and margin bands
//! (`listing_bounds`).
//!
//! From `V54` the toxicity score's tiers take the same notice, within ±50% of their launch values
//! (`toxicity`). From `V55` the profit fee moves only up from its launch value, and a notice may name
//! a new profit fee curve, at or above its launch curve at every hold (`holding_fee`). From `V59` a
//! notice may name a new volume tier table, within its launch bounds (`volume_tier`). From `V62` new
//! stress parameters keep each launch house loss threshold at its level or above, at most its launch
//! value (`house_loss`). From `V76` a notice may name a new rollover time (`rollover`) and new corporate
//! action bounds (`corporate::CorporateBounds`). From `V81` a notice may name a new portfolio grid,
//! moving only its shocks and floor shares up within their launch bounds (`portfolio`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::corporate::{CappedDividend, CorporateBounds};
use crate::engine::Engine;
use crate::holding_fee::{CurvePoint, spend_curve_notice};
use crate::house_loss::LossTrigger;
use crate::listing_bounds::{LaunchBands, ListingNotice};
use crate::market::MarketSpec;
use crate::params::{MAX_RATE_BPS, ProtocolParams, Rollover, StressLevel};
use crate::portfolio::{PortfolioGrid, spend_grid_notice};
use crate::protocol::ProtocolVersion;
use crate::toxicity::ScoreTier;
use crate::types::{AccountId, MAX_STAMP_MS, MarketId, Millis};
use crate::volume_tier::{VolumeTier, spend_volume_tier_notice};

mod held;
mod notices;
pub use crate::operator_bounds::notices::{NOTICE_USE_MS, lapse_of};
pub(crate) mod rollover;
mod rotation;

/// How long before a rate change or an operator rotation takes effect it is logged: seven days.
pub const NOTICE_MS: Millis = 7 * 24 * 60 * 60 * 1000;

/// The profit fee and close-out level the bounds are measured from: those in force when the
/// parameters are first set under `V35`, or when `V35` activates over parameters already set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchRates {
    pub profit_fee_bps: i64,
    pub closeout_level_bps: i64,
}

/// The operator's notice of new rates: each named value may take effect in a `SetParams` from
/// `effective_at` on, once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamsNotice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profit_fee_bps: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closeout_level_bps: Option<i64>,
    /// From `V49`, new stress parameters, left out of the encoding when none are named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stress: Option<StressSettings>,
    /// From `V54`, new toxicity score tiers, left out of the encoding when none are named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_tiers: Option<Vec<ScoreTier>>,
    /// From `V55`, a new profit fee curve, left out of the encoding when none is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profit_fee_curve: Option<Vec<CurvePoint>>,
    /// From `V59`, a new volume tier table, left out of the encoding when none is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_tiers: Option<Vec<VolumeTier>>,
    /// From `V76`, a new rollover time, left out of the encoding when none is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollover: Option<Rollover>,
    /// From `V76`, new corporate action bounds, left out of the encoding when none are named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corporate_bounds: Option<CorporateBounds>,
    /// From `V81`, a new portfolio grid, left out of the encoding when none is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portfolio_grid: Option<PortfolioGrid>,
    /// The sequenced time the new values may apply from.
    pub effective_at: Millis,
}

/// The stress parameters, which together decide when a market reaches a level and how long it is
/// held there: the levels with their spreads and triggers, the move window and the cooldown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StressSettings {
    pub levels: Vec<StressLevel>,
    pub move_window_ms: i64,
    pub cooldown_ms: i64,
}

impl StressSettings {
    /// The stress parameters `params` sets.
    pub fn of(params: &ProtocolParams) -> Self {
        Self {
            levels: params.stress_levels.clone(),
            move_window_ms: params.stress_move_window_ms,
            cooldown_ms: params.stress_cooldown_ms,
        }
    }
}

/// A rotation waiting out its notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRotation {
    pub address: AccountId,
    pub effective_at: Millis,
}

/// What the bounds keep in state, left out of the encoding while empty.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorBounds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchRates>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<ParamsNotice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<PendingRotation>,
    /// How many rotations have taken effect.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rotations: u64,
    /// From `V46`, when a global stress floor above level 0 was set, which starts its time to lapse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_floor_set_at: Option<Millis>,
    /// From `V49`, each listed market's pending listing notice: a markup, and from `V52` its spread
    /// and margin bands.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub markups: BTreeMap<MarketId, ListingNotice>,
    /// From `V52`, each listed market's launch bands, which bound what a listing notice may name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub launch_bands: BTreeMap<MarketId, LaunchBands>,
    /// From `V52`, the listing a ceiling adoption for a market not yet listed bound, which that
    /// market's first listing must be.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub proposed_listings: BTreeMap<MarketId, MarketSpec>,
    /// From `V54`, the toxicity score tiers the bounds are measured from: those of the first
    /// parameters set under `V54`, or of its activation over parameters already set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_score_tiers: Option<Vec<ScoreTier>>,
    /// From `V55`, the curve in force at the first parameters set under `V55` or at its activation over
    /// them: later curves stay at or above it, and from `V57` its last hold, if any, clamps an add's age.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_curve: Option<Vec<CurvePoint>>,
    /// From `V59`, the volume tier table in force when parameters are first set under `V59` or when it
    /// activates over them (from `V61` `launch_volume_tiers`, whether or not in force), which bounds every
    /// later table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_volume_tiers: Option<Vec<VolumeTier>>,
    /// From `V62`, the house loss thresholds in force when parameters are first set under `V62` or
    /// when it activates over them, which every later set of stress levels keeps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_loss_triggers: Option<Vec<LossTrigger>>,
    /// From `V76`, the corporate action bounds in force when parameters are first set under `V76` or when
    /// it activates over them, past which no later bounds loosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_corporate_bounds: Option<CorporateBounds>,
    /// From `V76`, when the last rollover charged, which bounds what the next one charges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rollover: Option<Millis>,
    /// From `V76`, the dividends scheduled on short notice, in the order scheduled, which the cap counts
    /// together; recording one drops those ex a week or more before it is scheduled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capped_dividends: Vec<CappedDividend>,
    /// From `V81`, the portfolio grid in force when parameters are first set under `V81` or when it
    /// activates over them, empty or not, which fixes every later grid's groups and bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_grid: Option<PortfolioGrid>,
}

impl OperatorBounds {
    /// Whether nothing is recorded, so earlier state encodings stand.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl ParamsNotice {
    /// Whether the notice names no value left to apply.
    fn names_nothing(&self) -> bool {
        self.profit_fee_bps.is_none()
            && self.closeout_level_bps.is_none()
            && self.stress.is_none()
            && self.score_tiers.is_none()
            && self.profit_fee_curve.is_none()
            && self.volume_tiers.is_none()
            && self.rollover.is_none()
            && self.corporate_bounds.is_none()
            && self.portfolio_grid.is_none()
    }
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

/// Whether `value` is within ±50% of `launch`.
pub(crate) fn within_half(value: i128, launch: i128) -> bool {
    value.checked_mul(2).is_some_and(|twice| twice >= launch && twice <= launch * 3)
}

impl Engine {
    /// The bounds' state: launch rates, the pending notice and rotation, and the rotations so far.
    pub fn operator_bounds(&self) -> &OperatorBounds {
        &self.operator_bounds
    }

    /// Records the rates in force as the launch rates when `V35` activates over parameters.
    pub(crate) fn record_launch_rates(&mut self) {
        if let (Some(params), None) = (&self.params, self.operator_bounds.launch) {
            self.operator_bounds.launch = Some(LaunchRates {
                profit_fee_bps: params.profit_fee_bps,
                closeout_level_bps: params.closeout_level_bps,
            });
        }
    }

    /// Records a notice of new rates, and from `V49` of new stress parameters, replacing any pending
    /// one.
    /// Error cases:
    /// - before `V35`, or before parameters are set
    /// - a notice naming no rate, a rate outside its valid range or outside ±50% of its launch value
    /// - stress parameters before `V49`, or ones that, with the other parameters in force, fail
    ///   `ProtocolParams::validate_for` or `ProtocolParams::check_closing_spread`, or from `V62` drop a
    ///   launch house loss threshold (`Engine::check_loss_triggers_kept`)
    /// - score tiers `Engine::check_score_tier_notice` refuses
    /// - from `V55`, a profit fee below its launch value; a curve `Engine::check_curve_notice` refuses
    /// - a volume tier table `Engine::check_volume_tier_notice` refuses
    /// - a rollover or corporate bounds `Engine::check_rollover_notice` or
    ///   `Engine::check_corporate_bounds_notice` refuses
    /// - a portfolio grid `Engine::check_grid_notice` refuses
    /// - an effective time less than seven days after the sequenced time, or past the latest stamp
    pub(crate) fn announce_params(&mut self, notice: &ParamsNotice) -> Result<(), String> {
        self.require_bounds_protocol("a parameter notice")?;
        let launch = self.operator_bounds.launch.ok_or("protocol params not set")?;
        if let Some(stress) = &notice.stress {
            if self.protocol_version < ProtocolVersion::V49 {
                return Err(format!("a stress notice needs protocol {:?}", ProtocolVersion::V49));
            }
            let current = self.params.as_ref().expect("launch rates are recorded with the parameters");
            let params = ProtocolParams {
                stress_levels: stress.levels.clone(),
                stress_move_window_ms: stress.move_window_ms,
                stress_cooldown_ms: stress.cooldown_ms,
                ..current.clone()
            };
            params.validate_for(self.protocol_version)?;
            params.check_closing_spread()?;
            self.check_loss_triggers_kept(&stress.levels)?;
        }
        if let Some(tiers) = &notice.score_tiers {
            self.check_score_tier_notice(tiers)?;
        }
        self.check_curve_notice(notice.profit_fee_curve.as_deref())?;
        self.check_volume_tier_notice(notice.volume_tiers.as_deref())?;
        self.check_rollover_notice(notice.rollover.as_ref())?;
        self.check_corporate_bounds_notice(notice.corporate_bounds.as_ref())?;
        self.check_grid_notice(notice.portfolio_grid.as_ref())?;
        if notice.names_nothing() {
            return Err("a parameter notice names no rate".into());
        }
        if let Some(fee) = notice.profit_fee_bps {
            if !(0..MAX_RATE_BPS).contains(&fee) {
                return Err(format!("profit_fee_bps {fee} must be in 0..10000"));
            }
            if !within_half(fee.into(), launch.profit_fee_bps.into()) {
                return Err(format!(
                    "profit_fee_bps {fee} is outside ±50% of its launch value {}",
                    launch.profit_fee_bps
                ));
            }
            self.check_fee_floor(fee, launch.profit_fee_bps)?;
        }
        if let Some(level) = notice.closeout_level_bps {
            if !(1..=MAX_RATE_BPS).contains(&level) {
                return Err(format!("closeout_level_bps {level} must be in 1..=10000"));
            }
            if !within_half(level.into(), launch.closeout_level_bps.into()) {
                return Err(format!(
                    "closeout_level_bps {level} is outside ±50% of its launch value {}",
                    launch.closeout_level_bps
                ));
            }
        }
        self.check_notice_time("a parameter notice", notice.effective_at)?;
        self.operator_bounds.notice = Some(notice.clone());
        Ok(())
    }

    /// From `V49`, records a notice of `market`'s new financing markup, replacing any pending one for
    /// that market.
    /// Error cases:
    /// - before `V49`, or for a market that is not listed
    /// - a markup outside ±10000 bps a year
    /// - an effective time less than seven days after the sequenced time, or past the latest stamp
    pub(crate) fn announce_markup(&mut self, market: MarketId, markup_bps: i64, at: Millis) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V49 {
            return Err(format!("a markup notice needs protocol {:?}", ProtocolVersion::V49));
        }
        if !self.markets.contains_key(&market) {
            return Err(format!("market {market} is not listed"));
        }
        if !(-MAX_RATE_BPS..=MAX_RATE_BPS).contains(&markup_bps) {
            return Err("financing.markup_bps must be within ±10000".into());
        }
        self.check_notice_time("a markup notice", at)?;
        self.operator_bounds.markups.insert(market, ListingNotice::markup(markup_bps, at));
        Ok(())
    }

    /// From `V49`, checks a relisting changes a listed market's markup only to the value its notice
    /// names, once that notice's time has come, and returns whether the relisting spends it. A `voted`
    /// relisting's notice is its vote's.
    /// Error cases:
    /// - a changed markup no notice names, or whose notice takes effect later than the sequenced time or
    ///   from `V88` has lapsed
    pub(crate) fn check_markup_change(&self, spec: &MarketSpec, voted: bool) -> Result<bool, String> {
        let Some(market) = self.markets.get(&spec.id) else { return Ok(false) };
        let (id, new) = (spec.id, spec.financing.markup_bps);
        if self.protocol_version < ProtocolVersion::V49 || market.spec.financing.markup_bps == new || voted {
            return Ok(false);
        }
        match self.operator_bounds.markups.get(&id).filter(|notice| notice.markup_bps == Some(new)) {
            None => Err(format!("market {id}'s markup change to {new} needs a notice of at least seven days")),
            Some(notice) => self
                .check_notice_in_force(notice.effective_at, &format!("market {id}'s markup change to {new}"))
                .map(|()| true),
        }
    }

    /// Checks a notice's time: at least seven days after the sequenced time, and no later than the
    /// latest stamp.
    pub(crate) fn check_notice_time(&self, what: &str, effective_at: Millis) -> Result<(), String> {
        if effective_at < self.now + NOTICE_MS {
            return Err(format!("{what} takes effect no sooner than {}", self.now + NOTICE_MS));
        }
        if effective_at > MAX_STAMP_MS {
            return Err(format!("{what} takes effect no later than {MAX_STAMP_MS}"));
        }
        Ok(())
    }

    /// Checks `params` against the bounds: a changed profit fee or close-out level is within ±50% of
    /// its launch value and named by a notice whose time has come, from `V49` so are changed stress
    /// parameters, with no range, and from `V55` a changed profit fee curve, within its bound. A
    /// `voted` change's notice is its vote's. Changes are measured from the baseline
    /// (`Engine::baseline_params`).
    /// Error cases:
    /// - a changed rate outside ±50% of its launch value, or from `V55` a profit fee below it
    /// - unless `voted`, a changed rate no notice names, or whose notice takes effect later than the
    ///   sequenced time or from `V88` has lapsed
    /// - from `V49`, unless `voted`, changed stress parameters no notice names, or whose notice takes
    ///   effect later than the sequenced time or from `V88` has lapsed
    /// - from `V62`, changed stress levels that drop a launch house loss threshold
    ///   (`Engine::check_loss_triggers_kept`)
    /// - from `V54`, changed score tiers `Engine::check_score_tier_change` refuses
    /// - a changed profit fee curve that fails `Engine::check_curve_change`
    /// - a changed volume tier table that fails `Engine::check_volume_tier_change`
    /// - a changed rollover or corporate bounds that fail `Engine::check_rollover_change` or
    ///   `Engine::check_corporate_bounds_change`
    /// - a changed portfolio grid that fails `Engine::check_grid_change`
    pub(crate) fn check_rate_changes(&self, params: &ProtocolParams, voted: bool) -> Result<(), String> {
        let Some(current) = &self.baseline_params() else { return Ok(()) };
        // Recorded with the first parameters under `V35`, or at its activation over them.
        let launch =
            self.operator_bounds.launch.expect("launch rates are recorded wherever parameters are set under V35");
        let rates = [
            ("profit_fee_bps", params.profit_fee_bps, current.profit_fee_bps, launch.profit_fee_bps),
            ("closeout_level_bps", params.closeout_level_bps, current.closeout_level_bps, launch.closeout_level_bps),
        ];
        for (name, new, old, launched) in rates {
            if new == old {
                continue;
            }
            if !within_half(new.into(), launched.into()) {
                return Err(format!("{name} {new} is outside ±50% of its launch value {launched}"));
            }
            if name == "profit_fee_bps" {
                self.check_fee_floor(new, launched)?;
            }
            if voted {
                continue;
            }
            let notice = self.operator_bounds.notice.as_ref().filter(|notice| match name {
                "profit_fee_bps" => notice.profit_fee_bps == Some(new),
                _ => notice.closeout_level_bps == Some(new),
            });
            match notice {
                None => return Err(format!("a {name} change to {new} needs a notice of at least seven days")),
                Some(notice) => {
                    self.check_notice_in_force(notice.effective_at, &format!("the {name} change to {new}"))?
                }
            }
        }
        self.check_score_tier_change(params, voted)?;
        self.check_curve_change(&params.profit_fee_curve, &current.profit_fee_curve, voted)?;
        self.check_volume_tier_change(params, current, voted)?;
        self.check_rollover_change(&params.rollover, &current.rollover, voted)?;
        self.check_corporate_bounds_change(&params.corporate_bounds, &current.corporate_bounds, voted)?;
        self.check_grid_change(&params.portfolio_grid, &current.portfolio_grid, voted)?;
        let stress = StressSettings::of(params);
        if self.protocol_version >= ProtocolVersion::V62 && params.stress_levels != current.stress_levels {
            self.check_loss_triggers_kept(&params.stress_levels)?;
        }
        if self.protocol_version < ProtocolVersion::V49 || stress == StressSettings::of(current) || voted {
            return Ok(());
        }
        match self.operator_bounds.notice.as_ref().filter(|notice| notice.stress.as_ref() == Some(&stress)) {
            None => Err("a stress parameter change needs a notice of at least seven days".into()),
            Some(notice) => self.check_notice_in_force(notice.effective_at, "the stress parameter change"),
        }
    }

    /// After new parameters apply: records the launch rates, score tiers, curve, volume tiers, corporate
    /// bounds and portfolio grid if these are the first, and spends the notice of each rate, of the
    /// stress parameters, of the score tiers, of the profit fee curve, of the volume tier table, of the
    /// rollover, of the corporate bounds and of the portfolio grid, that changed.
    pub(crate) fn settle_rate_changes(&mut self, before: Option<&ProtocolParams>, params: &ProtocolParams) {
        self.record_launch_rates();
        self.record_launch_score_tiers();
        self.record_launch_curve();
        self.record_launch_volume_tiers();
        self.record_launch_loss_triggers();
        self.record_launch_corporate_bounds();
        self.record_launch_grid();
        let Some(before) = before else { return };
        let tiers = &params.toxicity.score_tiers;
        let spends_tiers = *tiers != before.toxicity.score_tiers;
        let (fee, level, stress) = (before.profit_fee_bps, before.closeout_level_bps, StressSettings::of(before));
        let Some(notice) = self.operator_bounds.notice.as_mut() else { return };
        if params.profit_fee_bps != fee && notice.profit_fee_bps == Some(params.profit_fee_bps) {
            notice.profit_fee_bps = None;
        }
        if params.closeout_level_bps != level && notice.closeout_level_bps == Some(params.closeout_level_bps) {
            notice.closeout_level_bps = None;
        }
        let applied = StressSettings::of(params);
        if applied != stress && notice.stress.as_ref() == Some(&applied) {
            notice.stress = None;
        }
        if spends_tiers && notice.score_tiers.as_ref() == Some(tiers) {
            notice.score_tiers = None;
        }
        spend_curve_notice(notice, &before.profit_fee_curve, &params.profit_fee_curve);
        spend_volume_tier_notice(notice, &before.volume_tiers, &params.volume_tiers);
        if params.rollover != before.rollover && notice.rollover.as_ref() == Some(&params.rollover) {
            notice.rollover = None;
        }
        let bounds = Some(&params.corporate_bounds);
        if params.corporate_bounds != before.corporate_bounds && notice.corporate_bounds.as_ref() == bounds {
            notice.corporate_bounds = None;
        }
        spend_grid_notice(notice, &before.portfolio_grid, &params.portfolio_grid);
        if notice.names_nothing() {
            self.operator_bounds.notice = None;
        }
    }

    fn require_bounds_protocol(&self, what: &str) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V35 {
            return Err(format!("{what} needs protocol {:?}", ProtocolVersion::V35));
        }
        Ok(())
    }
}
