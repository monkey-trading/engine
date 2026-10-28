//! Authenticated, pre-close corporate instructions and deterministic ex-date entitlements.

mod calendar;
use crate::corporate::calendar::validate_boundaries;
mod cash;
mod split;
mod timeline;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::Engine;
use crate::event::Event;
use crate::market::MarketSpec;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, AssetClass, MAX_STAMP_MS, MarketId, Millis};

/// From `V33`, the most rollover and corporate-action boundaries one command catches up on before
/// its own stamp (the boundary at the stamp itself is extra). A command stamped further past the engine's schedule applies only these and is
/// deferred (`Event::CatchUpDeferred`); later commands continue from there, so a long gap or an
/// altered stamp never makes one `apply` walk it all. Daily rollovers and corporate boundaries
/// both count, so an outage of under a year with no corporate action defers nothing.
pub const MAX_CATCH_UP_BOUNDARIES: usize = 366;

/// One immutable operator instruction. Amounts are per share or index unit held at
/// `previous_close`, so a dividend going ex after a split is entered per post-split share.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorporateAction {
    pub id: String,
    pub market: MarketId,
    pub previous_close: Millis,
    pub ex_open: Millis,
    pub adjustment: Adjustment,
}

/// A cash dividend (including index dividend points) or an exact stock split.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Adjustment {
    Dividend {
        gross_per_unit: Fixed,
        /// Required signed input in [0, 1]; no default or inferred tax rate.
        withholding_rate: Fixed,
    },
    Split {
        new_units: u32,
        old_units: u32,
    },
}

/// Pending instruction and its quantities captured before any command at the close boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorporateEntitlement {
    pub instruction: CorporateAction,
    pub quantities: Option<BTreeMap<AccountId, Fixed>>,
    pub failure: Option<String>,
}

impl CorporateAction {
    /// Validates structural inputs independently of the current books and venue calendar.
    /// Error cases:
    /// - empty/oversized id, unordered/out-of-range boundaries, invalid amount/rate/ratio
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() || self.id.len() > 128 {
            return Err("corporate action id must contain 1–128 bytes".into());
        }
        if self.previous_close <= 0 || self.ex_open <= self.previous_close || self.ex_open > MAX_STAMP_MS {
            return Err("corporate action boundaries must be ordered positive engine timestamps".into());
        }
        match self.adjustment {
            Adjustment::Dividend { gross_per_unit, withholding_rate } => {
                if !gross_per_unit.is_valid_input() {
                    return Err("dividend gross_per_unit must be positive and within the input bound".into());
                }
                if !(Fixed::ZERO..=Fixed::ONE).contains(&withholding_rate) {
                    return Err("dividend withholding_rate must be within 0–100%".into());
                }
            }
            Adjustment::Split { new_units, old_units } => {
                if new_units == 0 || old_units == 0 || new_units == old_units {
                    return Err("split must name positive unequal new_units and old_units".into());
                }
            }
        }
        Ok(())
    }
}

impl Engine {
    /// Pending corporate instructions and captured prior-close quantities, committed by the state tree.
    pub fn corporate_actions(&self) -> &BTreeMap<String, CorporateEntitlement> {
        &self.corporate_actions
    }

    /// Installs an immutable, authenticated instruction before its eligibility cutoff.
    /// Error cases:
    /// - inactive rules, invalid/duplicate/late instruction, unsupported market or wrong boundaries
    pub(crate) fn schedule_corporate_action(
        &mut self,
        action: &CorporateAction,
        events: &mut Vec<Event>,
    ) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V23 {
            return Err("corporate actions need protocol V23".into());
        }
        action.validate()?;
        if self.params.is_none() {
            return Err("corporate actions need protocol parameters".into());
        }
        if self.now >= action.previous_close {
            return Err("corporate instruction arrived at or after its previous-close cutoff".into());
        }
        if self.corporate_refs.contains(&action.id) {
            return Err("duplicate corporate action id".into());
        }
        let market = self.markets.get(&action.market).ok_or("unknown corporate action market")?;
        let supported = match action.adjustment {
            Adjustment::Dividend { .. } => matches!(market.spec.asset_class, AssetClass::Equity | AssetClass::Index),
            Adjustment::Split { .. } => market.spec.asset_class == AssetClass::Equity,
        };
        if !supported {
            return Err("corporate adjustment is unsupported for this asset class".into());
        }
        if market.corporate_action_blocked {
            return Err("market is blocked by a failed split".into());
        }
        validate_boundaries(&market.spec.schedule, action.previous_close, action.ex_open)?;
        if self.corporate_actions.values().any(|pending| {
            pending.instruction.market == action.market
                && pending.instruction.ex_open == action.ex_open
                && (pending.instruction.previous_close != action.previous_close
                    || matches!(pending.instruction.adjustment, Adjustment::Split { .. })
                        == matches!(action.adjustment, Adjustment::Split { .. }))
        }) {
            return Err("duplicate or inconsistent corporate action at this market boundary".into());
        }
        self.corporate_refs.insert(action.id.clone());
        self.corporate_actions.insert(
            action.id.clone(),
            CorporateEntitlement { instruction: action.clone(), quantities: None, failure: None },
        );
        events.push(Event::CorporateActionScheduled { action: action.clone() });
        Ok(())
    }

    /// Refuses listing edits that would reinterpret a pending entitlement or its currency.
    /// Error cases:
    /// - a pending instruction's market changes calendar, class, currency, or symbol
    pub(crate) fn check_corporate_listing(&self, spec: &MarketSpec) -> Result<(), String> {
        let pending = self.corporate_actions.values().any(|p| p.instruction.market == spec.id);
        if pending
            && let Some(old) = self.markets.get(&spec.id)
            && (old.spec.schedule != spec.schedule
                || old.spec.asset_class != spec.asset_class
                || old.spec.quote_currency != spec.quote_currency
                || old.spec.symbol != spec.symbol)
        {
            return Err("pending corporate action locks its calendar, class, currency and symbol".into());
        }
        Ok(())
    }
}
