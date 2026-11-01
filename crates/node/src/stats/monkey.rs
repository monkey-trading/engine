//! `GET /v1/monkey`: the emission schedule and totals, the stake, and the distribution state.

use engine_core::emissions::{PERIOD_MS, PERIODS_PER_HALVING, TokenUnits};
use engine_core::staking::UsdcUnits;
use engine_core::types::Millis;
use engine_core::{Engine, Fixed};
use serde::Serialize;

use crate::stats::Stats;

#[derive(Serialize)]
pub struct MonkeyView {
    seq: Option<u64>,
    time_ms: Millis,
    /// Where the record behind `distribution.last_distribution` and the stake totals begins: a close
    /// before it is not in that field, though `distribution.last_closed_epoch` still counts it.
    begins_ms: Option<Millis>,
    begins_seq: Option<u64>,
    from_genesis: bool,
    emissions: Option<EmissionsView>,
    staking: Option<StakingView>,
    distribution: Option<DistributionView>,
}

#[derive(Serialize)]
struct EmissionsView {
    start_ms: Millis,
    period_ms: Millis,
    period_budget: TokenUnits,
    halving_periods: u64,
    /// The period the last command fell in, `None` before the start, and its budget.
    period: Option<u64>,
    current_budget: Option<TokenUnits>,
    next_halving_ms: Option<Millis>,
    max_supply: TokenUnits,
    allocated: TokenUnits,
    cancelled: TokenUnits,
}

#[derive(Serialize)]
struct StakingView {
    total_staked: TokenUnits,
    stakers: u64,
    cooldown_ms: Millis,
}

#[derive(Serialize)]
struct DistributionView {
    start_ms: Millis,
    epoch_ms: Millis,
    liquidity_floor: Fixed,
    nav: Fixed,
    /// What an epoch close would distribute at the last scan, in USD: house NAV and funded cash above
    /// the floor, less reversible principal and client debts (`Engine::distributable_excess`).
    excess_usd: Option<Fixed>,
    /// The seq of that scan.
    excess_seq: Option<u64>,
    /// The epoch the last command fell in.
    epoch: u64,
    last_closed_epoch: Option<u64>,
    last_closed_ms: Option<Millis>,
    next_close_ms: Millis,
    /// USDC base units allocated to stakers across every closed epoch.
    distributed_usdc_units: UsdcUnits,
    /// The last close this record saw, with what it measured.
    last_distribution: Option<LastDistribution>,
}

#[derive(Serialize)]
struct LastDistribution {
    epoch: u64,
    closed_ms: Millis,
    nav: Fixed,
    floor: Fixed,
    excess_usdc_units: UsdcUnits,
    allocated_usdc_units: UsdcUnits,
}

impl Stats {
    /// The MONKEY emission schedule, staking and distribution state.
    pub(crate) fn monkey(&self, engine: &Engine) -> MonkeyView {
        let inner = self.lock();
        let now = engine.now();
        let emissions = engine.emissions().map(|emissions| {
            let schedule = emissions.schedule;
            let period = schedule.period_at(now);
            EmissionsView {
                start_ms: schedule.start_ms,
                period_ms: PERIOD_MS,
                period_budget: schedule.period_budget,
                halving_periods: PERIODS_PER_HALVING,
                period,
                current_budget: period.map(|period| TokenUnits(schedule.budget(period))),
                next_halving_ms: period
                    .and_then(|period| (period / PERIODS_PER_HALVING + 1).checked_mul(PERIODS_PER_HALVING))
                    .and_then(|first| i64::try_from(first).ok())
                    .and_then(|first| first.checked_mul(PERIOD_MS))
                    .and_then(|offset| schedule.start_ms.checked_add(offset)),
                max_supply: TokenUnits(schedule.nominal_supply()),
                allocated: emissions.entitled,
                cancelled: emissions.cancelled,
            }
        });
        let staking = engine.staking().map(|staking| StakingView {
            total_staked: TokenUnits(inner.staked),
            stakers: inner.stakers,
            cooldown_ms: staking.config.cooldown_ms,
        });
        let distribution = engine.staking().map(|staking| {
            let config = staking.config;
            let epoch = config.epochs_elapsed(now);
            let nav = engine.house_nav();
            DistributionView {
                start_ms: config.start_ms,
                epoch_ms: config.epoch_ms,
                liquidity_floor: config.liquidity_floor,
                nav,
                excess_usd: inner.snapshot.as_ref().and_then(|snapshot| snapshot.distribution_excess),
                excess_seq: inner.snapshot.as_ref().map(|snapshot| snapshot.seq),
                epoch,
                last_closed_epoch: epoch.checked_sub(1),
                last_closed_ms: epoch.checked_sub(1).map(|closed| config.epoch_end(closed)),
                next_close_ms: config.epoch_end(epoch),
                distributed_usdc_units: staking.distributed,
                last_distribution: inner.distribution.map(|last| LastDistribution {
                    epoch: last.epoch,
                    closed_ms: last.closed_ms,
                    nav: last.nav,
                    floor: last.floor,
                    excess_usdc_units: UsdcUnits(last.excess),
                    allocated_usdc_units: UsdcUnits(last.allocated),
                }),
            }
        });
        MonkeyView {
            seq: engine.last_seq(),
            time_ms: now,
            begins_ms: inner.begin.map(|begin| begin.ts),
            begins_seq: inner.begin.map(|begin| begin.seq),
            from_genesis: inner.begins_at_genesis(),
            emissions,
            staking,
            distribution,
        }
    }
}
