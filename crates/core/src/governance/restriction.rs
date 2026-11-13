//! Fast-path restrictions: which commands one may carry, and the values in force while restrictions
//! hold, the stricter of the baseline and every restriction: the highest close-out level and margin
//! band rate, and the lowest house caps and floors. The engine keeps the values in force in the
//! parameters and market specs, so every reader prices with them, and the baseline beside them while
//! a restriction holds a field. The stress floor and reduce-only are read where they apply
//! (`Governance::stress_floor`, `Governance::reduce_only`), beside the operator's own.

use std::collections::BTreeSet;

use crate::Fixed;
use crate::command::AdminCommand;
use crate::engine::Engine;
use crate::event::Event;
use crate::governance::{Limit, MAX_RESTRICTIONS, MarketBaseline, ParamsBaseline, Restriction, command_hash};
use crate::market::{MarketSpec, Tier};
use crate::operator_bounds::within_half;
use crate::params::ProtocolParams;
use crate::types::MarketId;

/// What a fast-path proposal may carry, for its refusals.
const FAST_PATH: &str = "the fast path takes only a higher margin or close-out level, a stress floor, a lower house or market cap, or reduce-only";

impl ParamsBaseline {
    /// The restricted fields of `params`.
    fn of(params: &ProtocolParams) -> Self {
        Self {
            closeout_level_bps: params.closeout_level_bps,
            house_total_cap_bps: params.house_total_cap_bps,
            house_total_cap_floor: params.house_total_cap_floor,
        }
    }

    /// Writes these fields into `params`.
    fn write(&self, params: &mut ProtocolParams) {
        params.closeout_level_bps = self.closeout_level_bps;
        params.house_total_cap_bps = self.house_total_cap_bps;
        params.house_total_cap_floor = self.house_total_cap_floor;
    }
}

impl MarketBaseline {
    /// The restricted fields of `spec`.
    fn of(spec: &MarketSpec) -> Self {
        Self {
            margin_tiers: spec.margin_tiers.clone(),
            house_cap_bps: spec.house_cap_bps,
            house_cap_floor: spec.house_cap_floor,
        }
    }

    /// Writes these fields into `spec`.
    fn write(&self, spec: &mut MarketSpec) {
        spec.margin_tiers = self.margin_tiers.clone();
        spec.house_cap_bps = self.house_cap_bps;
        spec.house_cap_floor = self.house_cap_floor;
    }
}

impl Engine {
    /// The parameters as the baseline sets them: those in force with each restricted field's
    /// baseline.
    pub fn baseline_params(&self) -> Option<ProtocolParams> {
        let mut params = self.params.clone()?;
        if let Some(baseline) = &self.governance.params_baseline {
            baseline.write(&mut params);
        }
        Some(params)
    }

    /// `market`'s spec as the baseline sets it, when it is listed.
    pub fn baseline_spec(&self, market: MarketId) -> Option<MarketSpec> {
        let mut spec = self.markets.get(&market)?.spec.clone();
        if let Some(baseline) = self.governance.market_baselines.get(&market) {
            baseline.write(&mut spec);
        }
        Some(spec)
    }

    /// Applies a fast-path proposal as a restriction in force from now, renewing the one the same
    /// command set or any with the same limit, then reprices what it restricts: stress levels, and
    /// every account's margin.
    /// Error cases:
    /// - for a command no restriction in force came from, any error case of `Engine::limit_of`
    /// - `MAX_RESTRICTIONS` already in force, none of them identical
    pub(super) fn restrict_by_vote(
        &mut self,
        proposal: u64,
        command: &AdminCommand,
        events: &mut Vec<Event>,
    ) -> Result<(), String> {
        let command_hash = command_hash(command);
        // The command that set a restriction renews it, whatever the baseline has become since.
        let renewed = self.governance.restrictions.values().find(|held| held.command_hash == command_hash);
        let limit = match renewed {
            Some(held) => held.limit.clone(),
            None => self.limit_of(command)?,
        };
        let ttl = self.governance.config.expect("a mirrored proposal needs the recorded contract").restriction_ttl_ms;
        let restrictions = &mut self.governance.restrictions;
        restrictions.retain(|_, held| held.limit != limit);
        if restrictions.len() >= MAX_RESTRICTIONS {
            return Err(format!("{MAX_RESTRICTIONS} restrictions are already in force"));
        }
        let expires_at = self.now.saturating_add(ttl).min(crate::types::MAX_STAMP_MS);
        restrictions.insert(proposal, Restriction { limit, command_hash, expires_at });
        events.push(Event::RestrictionApplied { proposal, expires_at });
        self.restrict();
        self.restress_all(events);
        if self.params.is_some() {
            self.check_all_closeouts(events);
        }
        Ok(())
    }

    /// Checks new parameters keep every stress level a restriction in force holds a floor at, so no
    /// change lifts a restriction before its time.
    /// Error cases:
    /// - a restriction's floor at a level the parameters do not have
    pub(crate) fn check_restricted_levels(&self, params: &ProtocolParams) -> Result<(), String> {
        let held = self.governance.restrictions.iter().find_map(|(proposal, restriction)| match restriction.limit {
            Limit::StressFloor { level, .. } if level >= params.stress_levels.len() => Some((proposal, level)),
            _ => None,
        });
        match held {
            Some((proposal, level)) => Err(format!(
                "restriction {proposal} holds a floor at stress level {level}, which these parameters remove"
            )),
            None => Ok(()),
        }
    }

    /// Drops each restriction whose lifetime has run by the engine's time, restoring the baseline
    /// where no other restriction holds. Only ever less strict, so no close-out follows.
    pub(crate) fn lapse_restrictions(&mut self, events: &mut Vec<Event>) {
        let now = self.now;
        let lapsed: Vec<u64> = self
            .governance
            .restrictions
            .iter()
            .filter(|(_, restriction)| restriction.expires_at <= now)
            .map(|(proposal, _)| *proposal)
            .collect();
        if lapsed.is_empty() {
            return;
        }
        for proposal in lapsed {
            self.governance.restrictions.remove(&proposal);
            events.push(Event::RestrictionLapsed { proposal });
        }
        self.restrict();
        self.restress_all(events);
    }

    /// After new parameters, or a listing of `market`, set the baseline: takes it as the baseline
    /// of whatever a restriction holds, and puts the values in force back.
    pub(crate) fn rebase(&mut self, market: Option<MarketId>) {
        match market {
            None => {
                let Some(params) = &self.params else { return };
                if self.governance.params_baseline.is_some() {
                    self.governance.params_baseline = Some(ParamsBaseline::of(params));
                    self.restrict();
                }
            }
            Some(id) => {
                let Some(market) = self.markets.get(&id) else { return };
                if let Some(baseline) = self.governance.market_baselines.get_mut(&id) {
                    *baseline = MarketBaseline::of(&market.spec);
                    self.restrict();
                }
            }
        }
    }

    /// Sets the restricted fields in force to the stricter of the baseline and every restriction,
    /// recording the baseline while a restriction holds a field and dropping it once none does.
    fn restrict(&mut self) {
        let Engine { governance, params, markets, .. } = self;
        let restrictions = &governance.restrictions;
        if let Some(params) = params.as_mut() {
            let mut held = restrictions.values().filter(|r| matches!(r.limit, Limit::Params { .. })).peekable();
            if held.peek().is_none() {
                if let Some(baseline) = governance.params_baseline.take() {
                    baseline.write(params);
                }
            } else {
                let baseline = *governance.params_baseline.get_or_insert_with(|| ParamsBaseline::of(params));
                let mut strict = baseline;
                for restriction in held {
                    if let Limit::Params { closeout_level_bps, house_total_cap_bps, house_total_cap_floor } =
                        &restriction.limit
                    {
                        strict.closeout_level_bps = strict.closeout_level_bps.max(closeout_level_bps.unwrap_or(0));
                        strict.house_total_cap_bps = house_total_cap_bps
                            .map_or(strict.house_total_cap_bps, |cap| cap.min(strict.house_total_cap_bps));
                        strict.house_total_cap_floor = house_total_cap_floor
                            .map_or(strict.house_total_cap_floor, |floor| floor.min(strict.house_total_cap_floor));
                    }
                }
                if ParamsBaseline::of(params) != strict {
                    strict.write(params);
                }
            }
        }
        let held_markets: BTreeSet<MarketId> = restrictions
            .values()
            .filter_map(|r| match r.limit {
                Limit::Market { market, .. } => Some(market),
                _ => None,
            })
            .collect();
        let ids: BTreeSet<MarketId> =
            governance.market_baselines.keys().copied().chain(held_markets.iter().copied()).collect();
        for id in ids {
            let Some(market) = markets.get(&id) else { continue };
            if !held_markets.contains(&id) {
                let baseline =
                    governance.market_baselines.remove(&id).expect("ids hold only baselines and held markets");
                if MarketBaseline::of(&market.spec) != baseline {
                    baseline.write(&mut markets.get_mut(&id).expect("listed").spec);
                }
                continue;
            }
            let baseline =
                governance.market_baselines.entry(id).or_insert_with(|| MarketBaseline::of(&market.spec)).clone();
            let mut strict = baseline;
            for restriction in restrictions.values() {
                if let Limit::Market { market, margin_tiers, house_cap_bps, house_cap_floor } = &restriction.limit
                    && *market == id
                {
                    if let Some(tiers) = margin_tiers {
                        strict.margin_tiers = stricter_tiers(&strict.margin_tiers, tiers);
                    }
                    strict.house_cap_bps =
                        house_cap_bps.map_or(strict.house_cap_bps, |cap| cap.min(strict.house_cap_bps));
                    strict.house_cap_floor =
                        house_cap_floor.map_or(strict.house_cap_floor, |floor| floor.min(strict.house_cap_floor));
                }
            }
            if MarketBaseline::of(&market.spec) != strict {
                strict.write(&mut markets.get_mut(&id).expect("listed").spec);
            }
        }
    }

    /// The restriction a fast-path `command` holds, measured against the baseline.
    /// Error cases:
    /// - a command other than `SetParams`, `UpsertMarket`, `SetStressFloor` or `SetReduceOnly`
    /// - `SetParams` before parameters are set, changing a field other than the close-out level and the
    ///   total house cap and floor, lowering the close-out level or raising a cap, or failing
    ///   `ProtocolParams::validate_for`, or a close-out level outside ±50% of its launch value
    /// - `UpsertMarket` of a market not listed, changing a field other than the margin bands and the
    ///   market's house cap and floor, lowering any band's rate at any notional or raising a cap, or
    ///   failing `MarketSpec::validate`
    /// - `SetStressFloor` at level 0, at a level that does not exist, or for an unknown market
    /// - `SetReduceOnly` lifting reduce-only, or for an unknown market
    /// - a command that changes nothing
    fn limit_of(&self, command: &AdminCommand) -> Result<Limit, String> {
        let unknown = |market: Option<MarketId>| market.is_some_and(|id| !self.markets.contains_key(&id));
        let limit = match command {
            AdminCommand::SetParams(proposed) => {
                let baseline = self.baseline_params().ok_or("protocol params not set")?;
                proposed.validate_for(self.protocol_version)?;
                let mut others = proposed.clone();
                ParamsBaseline::of(&baseline).write(&mut others);
                if others != baseline {
                    return Err(format!("{FAST_PATH}: this SetParams changes another parameter"));
                }
                let (level, cap, floor) =
                    (proposed.closeout_level_bps, proposed.house_total_cap_bps, proposed.house_total_cap_floor);
                if level < baseline.closeout_level_bps
                    || cap > baseline.house_total_cap_bps
                    || floor > baseline.house_total_cap_floor
                {
                    return Err(format!(
                        "{FAST_PATH}: this SetParams lowers the close-out level or raises a house cap"
                    ));
                }
                let launch = self.operator_bounds.launch.ok_or("no launch rates are recorded")?;
                if !within_half(level.into(), launch.closeout_level_bps.into()) {
                    return Err(format!(
                        "closeout_level_bps {level} is outside ±50% of its launch value {}",
                        launch.closeout_level_bps
                    ));
                }
                Limit::Params {
                    closeout_level_bps: (level != baseline.closeout_level_bps).then_some(level),
                    house_total_cap_bps: (cap != baseline.house_total_cap_bps).then_some(cap),
                    house_total_cap_floor: (floor != baseline.house_total_cap_floor).then_some(floor),
                }
            }
            AdminCommand::UpsertMarket(proposed) => {
                let baseline = self.baseline_spec(proposed.id).ok_or("the fast path restricts listed markets only")?;
                proposed.validate()?;
                let mut others = proposed.clone();
                MarketBaseline::of(&baseline).write(&mut others);
                if others != baseline {
                    return Err(format!("{FAST_PATH}: this UpsertMarket changes another field"));
                }
                if !tiers_at_least(&proposed.margin_tiers, &baseline.margin_tiers)
                    || proposed.house_cap_bps > baseline.house_cap_bps
                    || proposed.house_cap_floor > baseline.house_cap_floor
                {
                    return Err(format!("{FAST_PATH}: this UpsertMarket lowers a margin band or raises a house cap"));
                }
                Limit::Market {
                    market: proposed.id,
                    margin_tiers: (proposed.margin_tiers != baseline.margin_tiers)
                        .then(|| proposed.margin_tiers.clone()),
                    house_cap_bps: (proposed.house_cap_bps != baseline.house_cap_bps).then_some(proposed.house_cap_bps),
                    house_cap_floor: (proposed.house_cap_floor != baseline.house_cap_floor)
                        .then_some(proposed.house_cap_floor),
                }
            }
            AdminCommand::SetStressFloor { market, level } => {
                let levels = self.params.as_ref().ok_or("protocol params not set")?.stress_levels.len();
                if *level == 0 || *level >= levels {
                    return Err(format!("a restricting stress floor is a level from 1 to {}", levels - 1));
                }
                if unknown(*market) {
                    return Err("unknown market".into());
                }
                Limit::StressFloor { market: *market, level: *level }
            }
            AdminCommand::SetReduceOnly { market, reduce_only } => {
                if !reduce_only {
                    return Err(format!("{FAST_PATH}: lifting reduce-only adds risk"));
                }
                if unknown(*market) {
                    return Err("unknown market".into());
                }
                Limit::ReduceOnly { market: *market }
            }
            _ => return Err(FAST_PATH.into()),
        };
        let unchanged = matches!(
            &limit,
            Limit::Params { closeout_level_bps: None, house_total_cap_bps: None, house_total_cap_floor: None }
                | Limit::Market { margin_tiers: None, house_cap_bps: None, house_cap_floor: None, .. }
        );
        if unchanged {
            return Err("the proposal restricts nothing the baseline does not".into());
        }
        Ok(limit)
    }
}

/// The band table that charges, on every slice of notional, the higher of `a`'s and `b`'s rates there:
/// one band per slice both tables' bounds cut, so every bound of each stays one, and a requirement
/// that rounds each band's charge up is never below either table's. Both are valid tables, so it is
/// too.
pub(crate) fn stricter_tiers(a: &[Tier], b: &[Tier]) -> Vec<Tier> {
    slices(a, b).map(|(up_to, rate_a, rate_b)| Tier { up_to, bps: rate_a.max(rate_b) }).collect()
}

/// Whether `a` charges at least `b`'s rate on every slice of notional.
fn tiers_at_least(a: &[Tier], b: &[Tier]) -> bool {
    slices(a, b).all(|(_, rate_a, rate_b)| rate_a >= rate_b)
}

/// The slices both tables' bounds cut notional into, each by its upper bound (`None` for the last,
/// unbounded one), with `a`'s and `b`'s rate on it.
fn slices<'t>(a: &'t [Tier], b: &'t [Tier]) -> impl Iterator<Item = (Option<Fixed>, i64, i64)> + 't {
    let bounds: BTreeSet<Fixed> = a.iter().chain(b).filter_map(|tier| tier.up_to).collect();
    let rate_at = |tiers: &[Tier], up_to: Option<Fixed>| {
        let band = tiers.iter().find(|tier| match (tier.up_to, up_to) {
            (None, _) => true,
            (Some(hi), Some(at)) => at <= hi,
            (Some(_), None) => false,
        });
        // Valid tables end unbounded, so every slice has a band.
        band.expect("a valid table ends unbounded").bps
    };
    bounds.into_iter().map(Some).chain([None]).map(move |up_to| (up_to, rate_at(a, up_to), rate_at(b, up_to)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed::Rounding;
    use crate::market::staircase;

    fn tiers(bands: &[(Option<i64>, i64)]) -> Vec<Tier> {
        bands.iter().map(|(up_to, bps)| Tier { up_to: up_to.map(Fixed::from_int), bps: *bps }).collect()
    }

    #[test]
    fn the_stricter_table_takes_each_slices_higher_rate_and_keeps_every_bound() {
        let a = tiers(&[(Some(100), 500), (Some(300), 1_000), (None, 2_000)]);
        let b = tiers(&[(Some(200), 800), (None, 1_500)]);
        assert_eq!(
            stricter_tiers(&a, &b),
            tiers(&[(Some(100), 800), (Some(200), 1_000), (Some(300), 1_500), (None, 2_000)])
        );
        assert_eq!(stricter_tiers(&a, &a), a);
        // Adjacent bands at one rate stay two bands: each one's charge rounds up on its own.
        let split = tiers(&[(Some(100), 700), (None, 700)]);
        assert_eq!(stricter_tiers(&split, &tiers(&[(None, 100)])), split);
    }

    #[test]
    fn the_stricter_table_never_requires_less_than_either_table_after_rounding() {
        let one_unit = Fixed::from_raw(1);
        let split = vec![Tier { up_to: Some(Fixed::ONE + one_unit), bps: 1_000 }, Tier { up_to: None, bps: 1_000 }];
        let whole = tiers(&[(None, 1_000)]);
        let banded = tiers(&[(Some(1), 900), (Some(3), 1_200), (None, 1_000)]);
        let tables = [split, whole, banded];
        let notionals = [1, 2, 3, 7]
            .map(Fixed::from_int)
            .into_iter()
            .chain([Fixed::ONE + one_unit, Fixed::from_raw(2_000_000_003)]);
        for notional in notionals {
            for a in &tables {
                for b in &tables {
                    let merged = stricter_tiers(a, b);
                    let required = |table: &[Tier]| staircase(table, Fixed::ZERO, notional, Rounding::Up);
                    assert!(required(&merged) >= required(a).max(required(b)), "{a:?} {b:?} at {notional}");
                }
            }
        }
    }

    #[test]
    fn at_least_compares_every_slice() {
        let base = tiers(&[(Some(100), 500), (None, 1_000)]);
        assert!(tiers_at_least(&tiers(&[(None, 1_000)]), &base));
        assert!(tiers_at_least(&tiers(&[(Some(50), 600), (None, 1_000)]), &base));
        assert!(!tiers_at_least(&tiers(&[(Some(200), 900), (None, 1_000)]), &base), "100 to 200 drops to 900");
        assert!(!tiers_at_least(&tiers(&[(None, 999)]), &base));
    }
}
