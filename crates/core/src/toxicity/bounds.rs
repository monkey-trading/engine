//! What bounds the score from `V54`: its activation, each market's floor, and the operator's changes
//! to the tiers. The horizon, half-life, fill minimum and weight cap are constants, so only a
//! protocol version changes them.

use crate::Fixed;
use crate::engine::Engine;
use crate::market::MarketSpec;
use crate::operator_bounds::within_half;
use crate::params::ProtocolParams;
use crate::protocol::ProtocolVersion;
use crate::toxicity::{HORIZON_MS, ScoreTier, launch_score_tiers, validate_score_tiers};
use crate::types::{AccountId, AssetClass, MarketId};

/// The markout floor `V54`'s activation gives a listing that names none, in bps: 0.1 for FX, bonds
/// and rates, 0.5 for indices, and 1 for the rest.
pub fn default_markout_floor_bps(class: AssetClass) -> Fixed {
    match class {
        AssetClass::Fx | AssetClass::Bond | AssetClass::Rates => Fixed::from_raw(100_000_000),
        AssetClass::Index => Fixed::from_raw(500_000_000),
        AssetClass::Commodity | AssetClass::Equity | AssetClass::Etf | AssetClass::Crypto => Fixed::ONE,
    }
}

impl ProtocolParams {
    /// Gives the parameters the form `V54` takes: no tiers of the earlier score, the protocol's
    /// horizon, and the score tiers they name, or the launch score tiers where they name none.
    pub fn convert_to_score(&mut self) {
        self.toxicity.tiers.clear();
        self.toxicity.horizon_ms = HORIZON_MS;
        if self.toxicity.score_tiers.is_empty() {
            self.toxicity.score_tiers = launch_score_tiers();
        }
    }
}

impl MarketSpec {
    /// Gives a listing that names no markout floor its asset class's default, as `V54`'s activation
    /// does.
    pub fn convert_to_floored(&mut self) {
        self.markout_floor_bps.get_or_insert(default_markout_floor_bps(self.asset_class));
    }
}

impl Engine {
    /// From `V8`, checks every closing trade can fill under the parameters set as an activation to
    /// `version` leaves them: in the form `V54` gives them (`ProtocolParams::convert_to_score`) when
    /// it carries the log from before `V54` to at or past it.
    /// Error cases:
    /// - the parameters as the activation leaves them fail `ProtocolParams::check_closing_spread`
    /// - carrying the log to `V54`, a listed market's ceiling or pending adoption below its launch
    ///   close charge under them, and carrying it to `V58`, a bound listing's (see
    ///   `Engine::check_ceilings_cover`)
    pub(crate) fn check_activation_closing_spread(&self, version: ProtocolVersion) -> Result<(), String> {
        let Some(params) = self.params.as_ref().filter(|_| version >= ProtocolVersion::V8) else { return Ok(()) };
        if self.protocol_version >= ProtocolVersion::V54 || version < ProtocolVersion::V54 {
            return params.check_closing_spread();
        }
        let mut converted = params.clone();
        converted.convert_to_score();
        converted.check_closing_spread()?;
        if self.protocol_version < ProtocolVersion::V52 {
            converted.raise_size_multipliers();
        }
        self.check_ceilings_cover(&converted, ProtocolVersion::V54, version)
    }

    /// Starts the score when an activation carries the log from before `V54` to `version` at or past
    /// it: drops every queued markout, clears every account's `V11` score, so every account starts
    /// neutral with its account leaf in the encoding it had with no score, gives parameters already
    /// set the form `V54` takes (`ProtocolParams::convert_to_score`), whose score tiers become the
    /// tiers the operator's bounds are measured from, and gives every listing that names no markout
    /// floor its asset class's default.
    pub(crate) fn start_toxicity_score(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V54 || version < ProtocolVersion::V54 {
            return;
        }
        // Nothing indexed them for matching: the index holds only `V54` markouts.
        while self.pop_markout().is_some() {}
        let unfloored: Vec<MarketId> = self
            .markets
            .iter()
            .filter(|(_, market)| market.spec.markout_floor_bps.is_none())
            .map(|(id, _)| *id)
            .collect();
        for id in unfloored {
            self.markets.get_mut(&id).expect("selected above").spec.convert_to_floored();
        }
        let scored: Vec<AccountId> = self
            .accounts
            .iter()
            .filter(|(_, acct)| {
                acct.toxicity_bps != 0 || !acct.toxicity_fraction.is_zero() || !acct.toxicity_history.is_zero()
            })
            .map(|(account, _)| *account)
            .collect();
        for account in scored {
            let acct = self.accounts.get_mut(&account).expect("selected above");
            (acct.toxicity_bps, acct.toxicity_fraction, acct.toxicity_history) = (0, Fixed::ZERO, Fixed::ZERO);
        }
        if let Some(params) = self.params.as_mut() {
            params.convert_to_score();
            self.operator_bounds.launch_score_tiers = Some(params.toxicity.score_tiers.clone());
        }
    }

    /// When an activation carries the log from before `V58` to `version` at or past it, gives each
    /// listing an adoption binds for a market not yet listed, applied or pending, that names no
    /// markout floor its asset class's default, as `V54`'s activation gives a listed market's: from
    /// `V54` a first listing names one, and it must be the listing its adoption bound.
    pub(crate) fn floor_bound_listings(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V58 || version < ProtocolVersion::V58 {
            return;
        }
        let listed = |id: MarketId| self.markets.contains_key(&id);
        let pending = self.pending_ceiling.as_mut().filter(|pending| !listed(pending.market));
        for spec in self.operator_bounds.proposed_listings.values_mut().chain(pending.and_then(|p| p.listing.as_mut()))
        {
            spec.convert_to_floored();
        }
    }

    /// From `V54`, records the score tiers in force as the launch tiers, once.
    pub(crate) fn record_launch_score_tiers(&mut self) {
        if self.protocol_version >= ProtocolVersion::V54
            && self.operator_bounds.launch_score_tiers.is_none()
            && let Some(params) = &self.params
        {
            self.operator_bounds.launch_score_tiers = Some(params.toxicity.score_tiers.clone());
        }
    }

    /// From `V54`, checks a listing names a markout floor, and a listed market the one it has. A
    /// listing before `V54` may name one for its activation to take up.
    /// Error cases:
    /// - from `V54`, a listing with no floor, or a listed market's floor changed
    pub(crate) fn check_markout_floor(&self, spec: &MarketSpec) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V54 {
            return Ok(());
        }
        let Some(floor) = spec.markout_floor_bps else {
            return Err(format!("market {} names no markout floor", spec.id));
        };
        match self.markets.get(&spec.id).and_then(|market| market.spec.markout_floor_bps) {
            Some(listed) if listed != floor => {
                Err(format!("market {}'s markout floor is fixed at {listed} bps", spec.id))
            }
            _ => Ok(()),
        }
    }

    /// Checks score tiers a parameter notice names.
    /// Error cases:
    /// - before `V54`
    /// - tiers that fail `toxicity::validate_score_tiers` or are outside the launch tiers' bounds (see
    ///   `check_within_launch`)
    pub(crate) fn check_score_tier_notice(&self, tiers: &[ScoreTier]) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V54 {
            return Err(format!("a score tier notice needs protocol {:?}", ProtocolVersion::V54));
        }
        validate_score_tiers(tiers)?;
        self.check_within_launch(tiers)
    }

    /// From `V54`, checks `params` change the score tiers in force only within the launch tiers'
    /// bounds and as a notice whose time has come names them. A `voted` change's notice is its vote's.
    /// Error cases:
    /// - changed tiers outside the launch tiers' bounds (see `check_within_launch`)
    /// - unless `voted`, changed tiers no notice names, or whose notice takes effect later than the
    ///   sequenced time
    pub(crate) fn check_score_tier_change(&self, params: &ProtocolParams, voted: bool) -> Result<(), String> {
        let Some(current) = &self.params else { return Ok(()) };
        let tiers = &params.toxicity.score_tiers;
        if self.protocol_version < ProtocolVersion::V54 || *tiers == current.toxicity.score_tiers {
            return Ok(());
        }
        self.check_within_launch(tiers)?;
        if voted {
            return Ok(());
        }
        let notice = self.operator_bounds.notice.as_ref().filter(|notice| notice.score_tiers.as_ref() == Some(tiers));
        match notice {
            None => Err("a score tier change needs a notice of at least seven days".into()),
            Some(notice) if notice.effective_at > self.now => {
                Err(format!("the score tier change takes effect from {}", notice.effective_at))
            }
            Some(_) => Ok(()),
        }
    }

    /// Checks `tiers` keep the launch tiers' count and each stays within ±50% of its launch tier's
    /// score and add-on; validation keeps every add-on at least 1 bp.
    /// Error cases:
    /// - a different number of tiers
    /// - a tier's score or add-on outside ±50% of its launch value
    fn check_within_launch(&self, tiers: &[ScoreTier]) -> Result<(), String> {
        let launch = self.operator_bounds.launch_score_tiers.as_deref().unwrap_or_default();
        if tiers.len() != launch.len() {
            return Err(format!("the score tiers keep their launch count of {}", launch.len()));
        }
        for (tier, launched) in tiers.iter().zip(launch) {
            if !within_half(tier.min_score.raw(), launched.min_score.raw())
                || !within_half(tier.extra_spread_bps.into(), launched.extra_spread_bps.into())
            {
                return Err(format!(
                    "a score tier at {} adding {} bps is outside ±50% of its launch tier at {} adding {} bps",
                    tier.min_score, tier.extra_spread_bps, launched.min_score, launched.extra_spread_bps
                ));
            }
        }
        Ok(())
    }
}
