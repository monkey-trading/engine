//! Protocol-wide parameters. Changed only by sequenced admin commands, so every change is in the
//! public log.

use chrono::{DateTime, Duration, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::fixed::Rounding;
use crate::market::MarketSpec;
use crate::protocol::ProtocolVersion;
use crate::types::Millis;

/// The most any per-trade rate may be: 100%.
pub(crate) const MAX_RATE_BPS: i64 = 10_000;
/// The largest benchmark rate accepted, either sign: 100% a year.
pub const MAX_BENCHMARK_BPS: i64 = 10_000;
/// The most a multiplier or house cap may be: ten times.
const MAX_MULTIPLE_BPS: i64 = 100_000;
/// The longest markout horizon: one hour. A horizon near `i64::MAX` overflows a markout's due time,
/// and before `V11` markouts are marked in fill order, so one queued under a long horizon holds back
/// every later one once the horizon is lowered; the cap bounds that wait.
const MAX_HORIZON_MS: i64 = 3_600_000;
/// The resting-order cap of parameters that name none, as those recorded before the cap existed.
/// It is protocol: changing it changes what those logs mean from `V14`.
pub const DEFAULT_MAX_RESTING_ORDERS: usize = 100;
/// The highest resting-order cap accepted. A price update checks every resting order in its market,
/// so the cap bounds what one account adds to that work.
const MAX_RESTING_ORDERS: usize = 1_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolParams {
    /// Fee on realised profit, in bps of the gain. Losing closes pay nothing.
    pub profit_fee_bps: i64,
    /// Account close-out when equity / margin requirement falls below this. The largest
    /// losing position is closed first.
    pub closeout_level_bps: i64,
    /// Cap on the sum of the house's absolute net notional across all markets, in bps of house NAV.
    pub house_total_cap_bps: i64,
    /// Index 0 is normal conditions. Higher levels widen spreads.
    pub stress_levels: Vec<StressLevel>,
    /// Window over which the stress engine measures a market's price move: before `V13`, from the
    /// first price of each window in turn; from `V13`, over the trailing window, which reaches back
    /// at least this far and less than a tenth of it further, or of the previous window for a while
    /// after it shortens.
    pub stress_move_window_ms: i64,
    /// Minimum time at a raised stress level before it can step back down.
    pub stress_cooldown_ms: i64,
    pub toxicity: ToxicityParams,
    pub rollover: Rollover,
    /// From protocol `V6`, the least the total house cap may be, in USD notional, however small or
    /// negative house NAV is. Zero means none, and is left out of the encoding.
    #[serde(default, skip_serializing_if = "Fixed::is_zero")]
    pub house_total_cap_floor: Fixed,
    /// Most resting orders one account may hold at once, across every market, from `V14`. Left out of
    /// the encoding at its default, so parameters that name none keep the roots they had.
    #[serde(default = "default_max_resting_orders", skip_serializing_if = "is_default_max_resting_orders")]
    pub max_resting_orders: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StressLevel {
    /// Multiplier on size-tier spreads, in bps (10_000 = 1x).
    pub size_spread_multiplier_bps: i64,
    /// Spread applied to every fill at this level, regardless of size.
    pub min_spread_bps: i64,
    /// Conditions that raise a market to this level automatically. Any one met is enough. `None`
    /// means operator-only.
    #[serde(default)]
    pub trigger: Option<StressTrigger>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StressTrigger {
    /// Venue divergence in bps of the index, before `V13`. Above a market's `max_divergence_bps` it
    /// can never fire there, since the market then has no price.
    #[serde(default)]
    pub divergence_bps: Option<i64>,
    #[serde(default)]
    pub move_bps: Option<i64>,
    /// House net exposure in the market as a share of its cap.
    #[serde(default)]
    pub utilisation_bps: Option<i64>,
    /// From `V13`, venue divergence as a share of the market's `max_divergence_bps`, so a share of
    /// up to 10_000, the bound itself, is reachable in every market.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub divergence_share_bps: Option<i64>,
}

/// Markout-based flow scoring. Each fill is marked against the index `horizon_ms` later; the
/// account's score averages those markouts in bps (positive means the trader won). Persistent
/// winners on short horizons are picking off stale prices and pay a wider spread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToxicityParams {
    pub horizon_ms: i64,
    /// Before `V11`, the weight of each new markout in the score's EWMA, in bps.
    pub ewma_weight_bps: i64,
    /// From `V11`, how a score weighs markouts by their fills' notional. Parameters set before `V11`
    /// have none, and are left out of the encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notional: Option<NotionalWeighting>,
    /// Ascending by `min_score_bps`; the highest tier reached applies.
    pub tiers: Vec<ToxicityTier>,
}

/// From `V11`, an account's toxicity score is the notional-weighted average of its fills' markouts
/// and of `prior` of neutral history at a score of zero, over at most `memory` of notional: past
/// that, each markout weighs its fill's notional against `memory` of history, and older markouts
/// fade. A single fill never sets a score, and dust barely moves one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotionalWeighting {
    /// The neutral history every account starts with, in USD of fill notional.
    pub prior: Fixed,
    /// The most history, in USD of fill notional, a score averages over.
    pub memory: Fixed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToxicityTier {
    pub min_score_bps: i64,
    pub extra_spread_bps: i64,
}

/// Daily financing rollover, e.g. 22:00 Europe/London.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rollover {
    pub tz: Tz,
    pub hour: u32,
    pub minute: u32,
}

impl ProtocolParams {
    /// Checks every parameter is in range.
    /// Error cases:
    /// - a profit fee outside 0..10000 bps, or a close-out level outside 1..=10000 bps
    /// - a total house cap outside 1..=100000 bps, or a total cap floor below zero or above
    ///   `Fixed::MAX_INPUT`
    /// - no stress levels, a trigger on level 0, a multiplier outside 0..=100000 bps, a minimum
    ///   spread outside 0..=10000 bps, a negative trigger threshold, or a divergence share above
    ///   10000 bps
    /// - a non-positive move window, or a negative cooldown
    /// - a toxicity horizon outside 1..=3600000 ms, an EWMA weight outside 1..=10000 bps, tiers
    ///   not strictly ascending, or an extra spread outside 0..=10000 bps
    /// - notional weighting that fails `NotionalWeighting::validate`
    /// - a rollover time that does not exist (hour above 23 or minute above 59)
    /// - a resting-order cap outside 1..=1000
    pub fn validate(&self) -> Result<(), String> {
        let rate = 0..=MAX_RATE_BPS;
        if !(0..MAX_RATE_BPS).contains(&self.profit_fee_bps) {
            return Err("profit_fee_bps must be in 0..10000".into());
        }
        if !(1..=MAX_RATE_BPS).contains(&self.closeout_level_bps) {
            return Err("closeout_level_bps must be in 1..=10000".into());
        }
        if !(1..=MAX_MULTIPLE_BPS).contains(&self.house_total_cap_bps) {
            return Err("house_total_cap_bps must be in 1..=100000".into());
        }
        if self.house_total_cap_floor.is_negative() || self.house_total_cap_floor > Fixed::MAX_INPUT {
            return Err("house_total_cap_floor out of range".into());
        }
        let Some(baseline) = self.stress_levels.first() else {
            return Err("stress_levels needs at least the normal level".into());
        };
        if baseline.trigger.is_some() {
            return Err("stress level 0 is the baseline and takes no trigger".into());
        }
        let bad_level = |l: &StressLevel| {
            !(0..=MAX_MULTIPLE_BPS).contains(&l.size_spread_multiplier_bps)
                || !rate.contains(&l.min_spread_bps)
                || l.trigger.as_ref().is_some_and(|t| {
                    [t.divergence_bps, t.move_bps, t.utilisation_bps, t.divergence_share_bps]
                        .iter()
                        .flatten()
                        .any(|x| *x < 0)
                        || t.divergence_share_bps.is_some_and(|x| x > MAX_RATE_BPS)
                })
        };
        if self.stress_levels.iter().any(bad_level) {
            return Err("stress level out of range".into());
        }
        if self.stress_move_window_ms <= 0 || self.stress_cooldown_ms < 0 {
            return Err("stress_move_window_ms must be > 0 and stress_cooldown_ms >= 0".into());
        }
        let tox = &self.toxicity;
        if !(1..=MAX_HORIZON_MS).contains(&tox.horizon_ms) || !(1..=MAX_RATE_BPS).contains(&tox.ewma_weight_bps) {
            return Err("toxicity needs horizon_ms in 1..=3600000 and ewma_weight_bps in 1..=10000".into());
        }
        if tox.tiers.windows(2).any(|w| w[0].min_score_bps >= w[1].min_score_bps)
            || tox.tiers.iter().any(|t| !rate.contains(&t.extra_spread_bps))
        {
            return Err("toxicity tiers must be ascending with extra_spread_bps in 0..=10000".into());
        }
        if let Some(notional) = &tox.notional {
            notional.validate()?;
        }
        if self.rollover.hour > 23 || self.rollover.minute > 59 {
            return Err("invalid rollover time".into());
        }
        if !(1..=MAX_RESTING_ORDERS).contains(&self.max_resting_orders) {
            return Err("max_resting_orders must be in 1..=1000".into());
        }
        Ok(())
    }

    /// Checks a trade that only reduces a position can always fill: the widest stress spread plus
    /// the widest toxicity spread stays under the price itself, since a reduction pays no size
    /// spread and a close-out no toxicity spread.
    /// Error cases:
    /// - a stress level's minimum spread and a toxicity tier's extra spread add up to 10000 bps or
    ///   more
    pub fn check_closing_spread(&self) -> Result<(), String> {
        let stress = self.stress_levels.iter().map(|level| level.min_spread_bps).max().unwrap_or(0);
        let toxicity = self.toxicity.tiers.iter().map(|tier| tier.extra_spread_bps).max().unwrap_or(0);
        if stress + toxicity >= MAX_RATE_BPS {
            return Err(format!(
                "stress and toxicity spreads of {stress} and {toxicity} bps reach the price, so no closing trade could fill"
            ));
        }
        Ok(())
    }

    /// Checks markouts are weighed by their fills' notional, as `V11` needs.
    /// Error cases:
    /// - no notional weighting
    pub fn check_notional_weighting(&self) -> Result<(), String> {
        if self.toxicity.notional.is_none() {
            return Err(format!("toxicity needs notional weighting from protocol {:?}", ProtocolVersion::V11));
        }
        Ok(())
    }

    /// Checks the parameters as `validate` does, and that each divergence trigger takes the form
    /// `version` evaluates: absolute bps before `V13`, and from `V13` a share of each market's bound.
    /// Error cases:
    /// - any case `validate` refuses
    /// - a divergence share before `V13`, or an absolute divergence from `V13`
    pub fn validate_for(&self, version: ProtocolVersion) -> Result<(), String> {
        self.validate()?;
        let mut triggers = self.stress_levels.iter().filter_map(|level| level.trigger.as_ref());
        if version >= ProtocolVersion::V13 {
            if triggers.any(|t| t.divergence_bps.is_some()) {
                return Err("from protocol V13 a divergence trigger is divergence_share_bps".into());
            }
        } else if triggers.any(|t| t.divergence_share_bps.is_some()) {
            return Err(format!("divergence_share_bps needs protocol {:?}", ProtocolVersion::V13));
        }
        Ok(())
    }

    /// Checks every divergence share can fire in `spec`'s market. A market with a zero divergence
    /// bound prices only while its venues agree, so its share is always zero.
    /// Error cases:
    /// - a divergence share above zero, with `spec`'s divergence bound at zero
    pub fn check_divergence_reachable(&self, spec: &MarketSpec) -> Result<(), String> {
        let positive_share = self
            .stress_levels
            .iter()
            .filter_map(|level| level.trigger.as_ref())
            .any(|t| t.divergence_share_bps.is_some_and(|share| share > 0));
        if positive_share && spec.price_rules.max_divergence_bps == 0 {
            return Err(format!(
                "a divergence share above zero can never fire in {}, whose bound is zero",
                spec.symbol
            ));
        }
        Ok(())
    }

    /// Turns each absolute divergence trigger into the same share of `widest_bound`, at most the
    /// bound itself: the form `V13` takes. Markets at the widest bound keep their thresholds and
    /// narrower ones scale down; the share truncates, so it fires no later than the threshold did.
    pub fn share_divergence_triggers(&mut self, widest_bound: i64) {
        for trigger in self.stress_levels.iter_mut().filter_map(|level| level.trigger.as_mut()) {
            if let Some(bps) = trigger.divergence_bps.take() {
                let share = bps.saturating_mul(MAX_RATE_BPS) / widest_bound.max(1);
                trigger.divergence_share_bps = Some(share.min(MAX_RATE_BPS));
            }
        }
    }

    /// Extra spread for an account with this toxicity score: the highest tier it reaches.
    pub fn toxicity_spread_bps(&self, score_bps: i64) -> i64 {
        self.toxicity.tiers.iter().rev().find(|t| score_bps >= t.min_score_bps).map_or(0, |t| t.extra_spread_bps)
    }
}

impl NotionalWeighting {
    /// Checks the prior is history and fits in the memory.
    /// Error cases:
    /// - a prior at or below zero, a memory below the prior, or a memory above `Fixed::MAX_INPUT`
    pub fn validate(&self) -> Result<(), String> {
        if !self.prior.is_positive() || self.memory < self.prior || self.memory > Fixed::MAX_INPUT {
            return Err(format!("toxicity notional needs 0 < prior <= memory <= {}", Fixed::MAX_INPUT));
        }
        Ok(())
    }

    /// `score`, with `history` of notional behind it, after a markout of `markout_bps` on a fill of
    /// `notional`: their notional-weighted average, and the history after it, at most `memory`.
    /// Rounds up, in the house's favour: a markout above the score always raises it, and none moves
    /// it past itself.
    pub(crate) fn averaged(&self, score: Fixed, history: Fixed, markout_bps: Fixed, notional: Fixed) -> (Fixed, Fixed) {
        // `history` is at least the positive prior, and the step is at most the markout's distance.
        let step = (markout_bps - score).mul_div(notional, history + notional, Rounding::Up);
        (score + step, (history + notional).min(self.memory))
    }
}

impl Rollover {
    /// The first rollover strictly after `ts`. In a DST gap, where the wall time does not exist,
    /// the rollover happens an hour later in wall-clock terms.
    pub fn next_after(&self, ts: Millis) -> Millis {
        // `Engine::apply` refuses stamps past `types::MAX_STAMP_MS`, so the calendar always has a
        // representable next day here, whatever the time zone or a DST gap does to the wall clock.
        let local = DateTime::<Utc>::from_timestamp_millis(ts).expect("engine time in range").with_timezone(&self.tz);
        let mut date = local.date_naive();
        loop {
            // `validate` rules out an hour or minute that does not exist.
            let naive = date.and_hms_opt(self.hour, self.minute, 0).expect("validated time");
            let at = self
                .tz
                .from_local_datetime(&naive)
                .earliest()
                .or_else(|| self.tz.from_local_datetime(&(naive + Duration::hours(1))).earliest());
            if let Some(at) = at.map(|t| t.timestamp_millis()).filter(|t| *t > ts) {
                return at;
            }
            date = date.succ_opt().expect("date in range");
        }
    }
}

/// The resting-order cap of parameters that name none.
fn default_max_resting_orders() -> usize {
    DEFAULT_MAX_RESTING_ORDERS
}

/// Whether `cap` is the default, which the encoding leaves out.
fn is_default_max_resting_orders(cap: &usize) -> bool {
    *cap == DEFAULT_MAX_RESTING_ORDERS
}

#[cfg(test)]
mod tests;
