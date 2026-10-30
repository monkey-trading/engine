//! Bounds on the operator's powers, from `V35`.
//!
//! The profit fee and the close-out level move only within ±50% of their launch values and only
//! after a notice: a signed `AnnounceParams` names the new values and the time they apply from, at
//! least seven days after it is logged, and a `SetParams` that carries one applies only from then.
//! A price-rule change that would leave a market with open positions without a price is refused
//! unless the market is reduce-only. The operator key rotates only by a signed `RotateOperator`,
//! which takes effect seven days after it is logged; from then the old key signs nothing.

use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::market::MarketSpec;
use crate::params::{MAX_RATE_BPS, ProtocolParams};
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MAX_STAMP_MS, Millis};

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamsNotice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profit_fee_bps: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closeout_level_bps: Option<i64>,
    /// The sequenced time the new values may apply from.
    pub effective_at: Millis,
}

/// A rotation waiting out its notice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRotation {
    pub address: AccountId,
    pub effective_at: Millis,
}

/// What the bounds keep in state, left out of the encoding while empty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl OperatorBounds {
    /// Whether nothing is recorded, so earlier state encodings stand.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

/// Whether `value` is within ±50% of `launch`.
fn within_half(value: i64, launch: i64) -> bool {
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

    /// Records a notice of new rates, replacing any pending one.
    /// Error cases:
    /// - before `V35`, or before parameters are set
    /// - a notice naming no rate, a rate outside its valid range or outside ±50% of its launch value
    /// - an effective time less than seven days after the sequenced time, or past the latest stamp
    pub(crate) fn announce_params(&mut self, notice: &ParamsNotice) -> Result<(), String> {
        self.require_bounds_protocol("a parameter notice")?;
        let launch = self.operator_bounds.launch.ok_or("protocol params not set")?;
        if notice.profit_fee_bps.is_none() && notice.closeout_level_bps.is_none() {
            return Err("a parameter notice names no rate".into());
        }
        if let Some(fee) = notice.profit_fee_bps {
            if !(0..MAX_RATE_BPS).contains(&fee) {
                return Err(format!("profit_fee_bps {fee} must be in 0..10000"));
            }
            if !within_half(fee, launch.profit_fee_bps) {
                return Err(format!(
                    "profit_fee_bps {fee} is outside ±50% of its launch value {}",
                    launch.profit_fee_bps
                ));
            }
        }
        if let Some(level) = notice.closeout_level_bps {
            if !(1..=MAX_RATE_BPS).contains(&level) {
                return Err(format!("closeout_level_bps {level} must be in 1..=10000"));
            }
            if !within_half(level, launch.closeout_level_bps) {
                return Err(format!(
                    "closeout_level_bps {level} is outside ±50% of its launch value {}",
                    launch.closeout_level_bps
                ));
            }
        }
        if notice.effective_at < self.now + NOTICE_MS {
            return Err(format!("a parameter notice takes effect no sooner than {}", self.now + NOTICE_MS));
        }
        if notice.effective_at > MAX_STAMP_MS {
            return Err(format!("a parameter notice takes effect no later than {MAX_STAMP_MS}"));
        }
        self.operator_bounds.notice = Some(*notice);
        Ok(())
    }

    /// Checks `params` against the bounds: a changed profit fee or close-out level is within ±50% of
    /// its launch value and named by a notice whose time has come.
    /// Error cases:
    /// - a changed rate outside ±50% of its launch value
    /// - a changed rate no notice names, or whose notice takes effect later than the sequenced time
    pub(crate) fn check_rate_changes(&self, params: &ProtocolParams) -> Result<(), String> {
        let Some(current) = &self.params else { return Ok(()) };
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
            if !within_half(new, launched) {
                return Err(format!("{name} {new} is outside ±50% of its launch value {launched}"));
            }
            let notice = self.operator_bounds.notice.filter(|notice| match name {
                "profit_fee_bps" => notice.profit_fee_bps == Some(new),
                _ => notice.closeout_level_bps == Some(new),
            });
            match notice {
                None => return Err(format!("a {name} change to {new} needs a notice of at least seven days")),
                Some(notice) if notice.effective_at > self.now => {
                    return Err(format!("the {name} change to {new} takes effect from {}", notice.effective_at));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// After new parameters apply: records the launch rates if these are the first, and spends the
    /// notice of each rate that changed.
    pub(crate) fn settle_rate_changes(&mut self, before: Option<(i64, i64)>, params: &ProtocolParams) {
        self.record_launch_rates();
        let (Some((fee, level)), Some(notice)) = (before, self.operator_bounds.notice.as_mut()) else { return };
        if params.profit_fee_bps != fee && notice.profit_fee_bps == Some(params.profit_fee_bps) {
            notice.profit_fee_bps = None;
        }
        if params.closeout_level_bps != level && notice.closeout_level_bps == Some(params.closeout_level_bps) {
            notice.closeout_level_bps = None;
        }
        if notice.profit_fee_bps.is_none() && notice.closeout_level_bps.is_none() {
            self.operator_bounds.notice = None;
        }
    }

    /// Records a rotation to `address`, replacing any pending one, from seven days after now.
    /// Error cases:
    /// - before `V35`, or with no operator recorded
    /// - from `V37`, while rotation is on, when only an election moves the key
    /// - from `V37`, while an uncancelled elected handover waits to take effect, even with rotation
    ///   off: only that handover moves the key until it applies or the registry cancels it
    /// - the zero address, or the address already operating
    pub(crate) fn rotate_operator(&mut self, address: AccountId) -> Result<(), String> {
        self.require_bounds_protocol("an operator rotation")?;
        if self.rotation_by_election() {
            return Err("while rotation is on, only an election moves the operator key".into());
        }
        let operator = self.operator.ok_or("no operator is recorded")?;
        if address == AccountId([0; 20]) || address == operator.address {
            return Err("an operator rotation names a new, nonzero address".into());
        }
        if self.operator_bounds.rotation.is_some_and(|rotation| self.is_elected(rotation)) {
            return Err("while an elected handover waits, only it moves the operator key".into());
        }
        self.operator_bounds.rotation = Some(PendingRotation { address, effective_at: self.now + NOTICE_MS });
        Ok(())
    }

    /// Hands the operator's role to the pending rotation's address once it is due (see
    /// `Engine::due_rotation`), before the command stamped `ts` is authorised, so the old key signs
    /// nothing from then.
    pub(crate) fn apply_due_rotation(&mut self, ts: Millis) {
        let Some(rotation) = self.due_rotation(ts) else {
            return;
        };
        // A rotation is recorded only beside a recorded operator, which is never removed.
        self.operator.as_mut().expect("a rotation needs a recorded operator").address = rotation.address;
        self.operator_bounds.rotation = None;
        self.operator_bounds.rotations += 1;
    }

    /// Checks a relisting leaves priced the markets that hold positions and depend on its price: the
    /// market itself, and the markets that convert at it. Price rules no tighter than those in force
    /// (no more sources, no shorter freshness, no narrower divergence) always do. Tighter ones must
    /// still give the market an index, from the quotes it keeps through the relisting, as trading
    /// reads it, unless every affected market that holds positions is reduce-only.
    /// Error cases:
    /// - tighter price rules that give no index, in a market that holds positions and accepts new
    ///   risk, or that markets holding positions and accepting new risk convert at
    pub(crate) fn check_priceable(&self, spec: &MarketSpec) -> Result<(), String> {
        let Some(market) = self.markets.get(&spec.id) else { return Ok(()) };
        let (old, new) = (&market.spec.price_rules, &spec.price_rules);
        let tighter = new.min_sources > old.min_sources
            || new.max_age_ms < old.max_age_ms
            || new.max_divergence_bps < old.max_divergence_bps;
        if !tighter {
            return Ok(());
        }
        // The market as the relisting leaves it: `UpsertMarket` drops the quotes stale under the old
        // limit when a relisting raises it.
        let mut relisted = market.clone();
        relisted.spec = spec.clone();
        if self.protocol_version >= ProtocolVersion::V33 && new.max_age_ms > old.max_age_ms {
            relisted.drop_quotes_older_than(self.now, old.max_age_ms);
        }
        let index = relisted.index(self.now).ok();
        let holders = &self.hot_indexes.as_ref().expect("apply builds derived indexes").holders;
        for affected in self.markets.values() {
            let own = affected.spec.id == spec.id;
            let converts = affected.spec.quote_currency.fx_market() == Some(spec.id);
            if !own && !converts {
                continue;
            }
            // No index prices nothing; an index too high to convert a currency prices nothing in it.
            let unpriced = match index {
                None => true,
                Some(index) => converts && !affected.spec.quote_currency.converts_at(index.mid),
            };
            let held = holders.get(&affected.spec.id).is_some_and(|accounts| !accounts.is_empty());
            if !unpriced || !held || self.is_reduce_only(affected) {
                continue;
            }
            return Err(if own {
                format!(
                    "market {} has open positions, so price rules it cannot price under need it reduce-only first",
                    spec.id
                )
            } else {
                format!(
                    "market {} has open positions and converts at market {}, which the price rules cannot price under, so it needs to be reduce-only first",
                    affected.spec.id, spec.id
                )
            });
        }
        Ok(())
    }

    fn require_bounds_protocol(&self, what: &str) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V35 {
            return Err(format!("{what} needs protocol {:?}", ProtocolVersion::V35));
        }
        Ok(())
    }
}
