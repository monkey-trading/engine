//! Corporate action bounds, from `V76`. A cash dividend scheduled less than seven days before its
//! previous close, and not by a passed vote, is held to `max_dividend_bps` of its market's mark when it
//! is scheduled, in the units held at its capture, together with every other such dividend on the
//! market whose ex-date open is within seven days of its own. A split's ratio is at most
//! `max_split_ratio` either way; it needs scheduling at least seven days before its previous close, or a
//! passed vote, since the venues' prices do not follow a split no issuer declared; and it applies after
//! the capture of every dividend pending on its market. And every action leaves at least
//! `min_trading_days` of the market's trading days, by its own calendar, before its ex-date: sessions
//! closing after it is scheduled, its previous close the last, on as many distinct dates, and a day of
//! time for each past the first. While votes govern, only a passed vote schedules a corporate action
//! (`Engine::votes_carry`). The bounds are parameters: they move only as a parameter notice of at least
//! seven days names them, or by a vote, and never looser than launch's, those in force when parameters
//! are first set under `V76` or when it activates over them. Instructions pending at the activation keep
//! the rules they were scheduled under.

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::corporate::calendar::trading_days_at_least;
use crate::corporate::{Adjustment, CorporateAction};
use crate::engine::{Engine, MarketState};
use crate::fixed::Rounding;
use crate::operator_bounds::NOTICE_MS;
use crate::params::MAX_RATE_BPS;
use crate::protocol::ProtocolVersion;
use crate::types::{MarketId, Millis};

/// The bounds at launch: a dividend of up to 10% of the mark, a split of up to 20 to 1 either way, and
/// two trading days.
pub const LAUNCH_CORPORATE_BOUNDS: CorporateBounds =
    CorporateBounds { max_dividend_bps: 1_000, max_split_ratio: 20, min_trading_days: 2 };
/// The widest split ratio the bounds may name.
const MAX_SPLIT_RATIO: u32 = 1_000;
/// The most trading days the bounds may ask for, which bounds the calendar walk that counts them.
const MAX_TRADING_DAYS: u32 = 20;

/// A dividend scheduled on short notice, under the cap: its market, ex-date open and share of the
/// market's mark, which every later one on the market within seven days of it counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CappedDividend {
    pub market: MarketId,
    pub ex_open: Millis,
    pub share: Fixed,
}

/// What a corporate action may be without more notice or a vote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorporateBounds {
    /// The most the cash dividends a market pays on short notice, less than seven days before their
    /// previous closes, total within seven days, in bps of the market's mark when each is scheduled.
    pub max_dividend_bps: i64,
    /// The widest split, in new units per old unit or old units per new unit.
    pub max_split_ratio: u32,
    /// The fewest of the market's trading days that close after an action is scheduled, its previous
    /// close the last of them: distinct dates, a day of time apart past the first.
    pub min_trading_days: u32,
}

impl Default for CorporateBounds {
    fn default() -> Self {
        LAUNCH_CORPORATE_BOUNDS
    }
}

impl CorporateBounds {
    /// Checks each bound is in range.
    /// Error cases:
    /// - a dividend share outside 1..=10000 bps, a split ratio outside 1..=1000, or trading days outside
    ///   1..=20
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_RATE_BPS).contains(&self.max_dividend_bps) {
            return Err("corporate_bounds.max_dividend_bps must be in 1..=10000".into());
        }
        if !(1..=MAX_SPLIT_RATIO).contains(&self.max_split_ratio) {
            return Err(format!("corporate_bounds.max_split_ratio must be in 1..={MAX_SPLIT_RATIO}"));
        }
        if !(1..=MAX_TRADING_DAYS).contains(&self.min_trading_days) {
            return Err(format!("corporate_bounds.min_trading_days must be in 1..={MAX_TRADING_DAYS}"));
        }
        Ok(())
    }

    /// Whether these bounds are at least as strict as `launch` in every bound.
    fn within(&self, launch: &CorporateBounds) -> bool {
        self.max_dividend_bps <= launch.max_dividend_bps
            && self.max_split_ratio <= launch.max_split_ratio
            && self.min_trading_days >= launch.min_trading_days
    }
}

/// Whether `bounds` are launch's, which the parameters' encoding leaves out.
pub(crate) fn is_launch(bounds: &CorporateBounds) -> bool {
    *bounds == LAUNCH_CORPORATE_BOUNDS
}

impl Engine {
    /// Checks the corporate bounds a notice names, if any.
    /// Error cases:
    /// - before `V76`
    /// - bounds `CorporateBounds::validate` refuses, or looser than launch's in any bound
    pub(crate) fn check_corporate_bounds_notice(&self, bounds: Option<&CorporateBounds>) -> Result<(), String> {
        let Some(bounds) = bounds else { return Ok(()) };
        if self.protocol_version < ProtocolVersion::V76 {
            return Err(format!("a corporate bounds notice needs protocol {:?}", ProtocolVersion::V76));
        }
        bounds.validate()?;
        self.check_corporate_bounds_within_launch(bounds)
    }

    /// From `V76`, checks parameters change the corporate bounds only to ones within launch's, named by
    /// a notice whose time has come. A `voted` change's notice is its vote's.
    /// Error cases:
    /// - from `V76`, changed bounds looser than launch's in any bound
    /// - from `V76`, unless `voted`, changed bounds no notice names, or whose notice takes effect later
    ///   than the sequenced time
    pub(crate) fn check_corporate_bounds_change(
        &self,
        bounds: &CorporateBounds,
        current: &CorporateBounds,
        voted: bool,
    ) -> Result<(), String> {
        if bounds == current || self.protocol_version < ProtocolVersion::V76 {
            return Ok(());
        }
        self.check_corporate_bounds_within_launch(bounds)?;
        if voted {
            return Ok(());
        }
        let named =
            self.operator_bounds.notice.as_ref().filter(|notice| notice.corporate_bounds.as_ref() == Some(bounds));
        match named {
            None => Err("a corporate bounds change needs a notice of at least seven days".into()),
            Some(notice) if notice.effective_at > self.now => {
                Err(format!("the corporate bounds change takes effect from {}", notice.effective_at))
            }
            Some(_) => Ok(()),
        }
    }

    /// Checks `bounds` are no looser than launch's.
    /// Error cases:
    /// - a bound looser than launch's
    fn check_corporate_bounds_within_launch(&self, bounds: &CorporateBounds) -> Result<(), String> {
        // Recorded with the first parameters under `V76`, or at its activation over them.
        let launch = self
            .operator_bounds
            .launch_corporate_bounds
            .expect("the launch corporate bounds are recorded wherever parameters are set under V76");
        if !bounds.within(&launch) {
            return Err(format!("corporate bounds {bounds:?} are looser than launch's {launch:?}"));
        }
        Ok(())
    }

    /// From `V76`, records the corporate bounds in force as launch's with the first parameters set under
    /// it; its activation over parameters already set records them there (`start_corporate_bounds`).
    pub(crate) fn record_launch_corporate_bounds(&mut self) {
        if let (Some(params), None) = (&self.params, &self.operator_bounds.launch_corporate_bounds)
            && self.protocol_version >= ProtocolVersion::V76
        {
            self.operator_bounds.launch_corporate_bounds = Some(params.corporate_bounds);
        }
    }

    /// When `version` activates `V76` over parameters already set, records the corporate bounds in force,
    /// launch's by then, as launch's.
    pub(crate) fn start_corporate_bounds(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V76 || version < ProtocolVersion::V76 {
            return;
        }
        if let Some(params) = &self.params {
            self.operator_bounds.launch_corporate_bounds = Some(params.corporate_bounds);
        }
    }

    /// From `V76`, checks `action` on `market` against the corporate bounds in force, and returns the
    /// short-notice dividend it is, if any, for `Engine::record_capped_dividend` once it is installed.
    /// A `voted` dividend's notice is its vote's.
    /// Error cases:
    /// - a dividend scheduled less than seven days before its previous close and not `voted` that
    ///   `Engine::check_dividend_cap` refuses
    /// - a split whose new or old units are more than the bound's ratio times the other, one scheduled
    ///   less than seven days before its previous close and not `voted`, or one that would apply at or
    ///   before the previous close of a dividend pending on its market
    /// - fewer than the bound's trading days before the previous close (`trading_days_at_least`)
    pub(super) fn check_corporate_action_bounds(
        &self,
        action: &CorporateAction,
        market: &MarketState,
        voted: bool,
    ) -> Result<Option<CappedDividend>, String> {
        if self.protocol_version < ProtocolVersion::V76 {
            return Ok(None);
        }
        let bounds = self.params.as_ref().expect("corporate actions need parameters").corporate_bounds;
        let capped = match action.adjustment {
            Adjustment::Dividend { gross_per_unit, .. } => {
                let noticed = action.previous_close - self.now >= NOTICE_MS;
                let capped =
                    (!noticed && !voted).then(|| self.check_dividend_cap(action, market, gross_per_unit, &bounds));
                capped.transpose()?
            }
            Adjustment::Split { new_units, old_units } => {
                let ratio = u64::from(bounds.max_split_ratio);
                if u64::from(new_units) > ratio * u64::from(old_units)
                    || u64::from(old_units) > ratio * u64::from(new_units)
                {
                    return Err(format!("a split's ratio is at most {ratio} either way"));
                }
                // An issuer announces a split well ahead, and the venues' prices do not follow one that no
                // issuer declared, so the split itself would pay its holders.
                if action.previous_close - self.now < NOTICE_MS && !voted {
                    return Err("a split needs a notice of at least seven days before its previous close".into());
                }
                // A pending dividend is per unit held at its capture, and its cap was measured so.
                let pending = self.corporate_actions.values().map(|pending| &pending.instruction);
                if pending.filter(|pending| pending.market == action.market).any(|pending| {
                    matches!(pending.adjustment, Adjustment::Dividend { .. })
                        && action.ex_open <= pending.previous_close
                }) {
                    return Err("a split may not apply before a pending dividend's previous close".into());
                }
                None
            }
        };
        if !trading_days_at_least(&market.spec.schedule, self.now, action.previous_close, bounds.min_trading_days) {
            return Err(format!(
                "a corporate action needs at least {} of its market's trading days after it is scheduled, its previous close the last",
                bounds.min_trading_days
            ));
        }
        Ok(capped)
    }

    /// Checks a short-notice dividend of `gross` per unit on `action`'s market, with every other one
    /// there whose ex-date open is within seven days of its own, totals at most the cap's share of the
    /// market's mark, in the units held at its capture: the mark scaled by each pending split that applies
    /// by its previous close. Returns the dividend with its share, rounded up.
    /// Error cases:
    /// - no mark, a mark the pending splits take past what `Fixed` holds, or a share that with those
    ///   dividends' is above `max_dividend_bps`
    fn check_dividend_cap(
        &self,
        action: &CorporateAction,
        market: &MarketState,
        gross: Fixed,
        bounds: &CorporateBounds,
    ) -> Result<CappedDividend, String> {
        let refused = || {
            format!(
                "dividends on short notice total at most {}bps of their market's mark within seven days; more needs a notice of at least seven days before the previous close",
                bounds.max_dividend_bps
            )
        };
        // Each split as it goes ex, in turn, and a mark it would take past what `Fixed` holds caps nothing.
        let mut splits: Vec<(Millis, Fixed, Fixed)> = self
            .corporate_actions
            .values()
            .filter_map(|pending| match pending.instruction.adjustment {
                Adjustment::Split { new_units, old_units }
                    if pending.instruction.market == action.market
                        && pending.instruction.ex_open <= action.previous_close =>
                {
                    let (old, new) = (Fixed::from_int(old_units.into()), Fixed::from_int(new_units.into()));
                    Some((pending.instruction.ex_open, old, new))
                }
                _ => None,
            })
            .collect();
        splits.sort_by_key(|(ex_open, ..)| *ex_open);
        let mark = market.mark.ok_or("the market has no mark to cap a short-notice dividend against")?;
        let mark = splits
            .into_iter()
            .try_fold(mark, |mark, (_, old, new)| mark.checked_mul_div(old, new, Rounding::Down))
            .ok_or_else(refused)?;
        let share = (mark.is_positive())
            .then(|| gross.checked_mul_div(Fixed::ONE, mark, Rounding::Up))
            .flatten()
            .ok_or_else(refused)?;
        let earlier: Fixed = self
            .operator_bounds
            .capped_dividends
            .iter()
            .filter(|capped| capped.market == action.market && (capped.ex_open - action.ex_open).abs() < NOTICE_MS)
            .map(|capped| capped.share)
            .sum();
        if earlier + share > Fixed::from_bps(bounds.max_dividend_bps) {
            return Err(refused());
        }
        Ok(CappedDividend { market: action.market, ex_open: action.ex_open, share })
    }

    /// Records a short-notice dividend `schedule_corporate_action` installed, and drops those no later
    /// one can be within seven days of, their ex-date opens a week or more past.
    pub(super) fn record_capped_dividend(&mut self, capped: Option<CappedDividend>) {
        let Some(capped) = capped else { return };
        let now = self.now;
        self.operator_bounds.capped_dividends.retain(|earlier| earlier.ex_open > now - NOTICE_MS);
        self.operator_bounds.capped_dividends.push(capped);
    }
}
