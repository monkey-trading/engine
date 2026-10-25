//! The emission schedule's arithmetic: period boundaries, halvings every 26,280 periods with budgets
//! rounded down, sums over long gaps, the nominal maximum supply, and token amounts as decimal
//! strings. The halving is also checked end to end through `Engine::apply`.

mod common;

use common::emissions::{allocations, cancelled_ranges, closed, funded, round_trip, schedule};
use common::{Harness, f};
use engine_core::emissions::{EmissionSchedule, MAX_PERIOD_BUDGET, PERIOD_MS, PERIODS_PER_HALVING, TokenUnits};

const E: u64 = PERIODS_PER_HALVING;

fn at(start_ms: i64, budget: u128) -> EmissionSchedule {
    EmissionSchedule { start_ms, period_budget: TokenUnits(budget) }
}

#[test]
fn periods_are_half_open_ten_minute_windows_from_the_start() {
    let schedule = at(1_000, 7);
    assert_eq!(PERIOD_MS, 600_000);
    assert_eq!(schedule.period_at(999), None);
    assert_eq!(schedule.period_at(1_000), Some(0));
    assert_eq!(schedule.period_at(1_000 + PERIOD_MS - 1), Some(0));
    assert_eq!(schedule.period_at(1_000 + PERIOD_MS), Some(1));
    assert_eq!(schedule.period_end(0), 1_000 + PERIOD_MS);
    // 182.5 days of ten-minute periods between halvings.
    assert_eq!(E as i64 * PERIOD_MS, 182 * 86_400_000 + 43_200_000);
}

#[test]
fn budgets_halve_every_26280_periods_rounding_down_until_zero() {
    let schedule = at(0, 3);
    let budgets: Vec<u128> = [0, E - 1, E, 2 * E - 1, 2 * E, 10 * E].map(|period| schedule.budget(period)).into();
    assert_eq!(budgets, [3, 3, 1, 1, 0, 0]);
    let largest = at(0, MAX_PERIOD_BUDGET);
    assert_eq!(largest.budget(127 * E), MAX_PERIOD_BUDGET >> 127);
    assert_eq!(largest.budget(128 * E), 0);
    assert_eq!(largest.budget(u64::MAX), 0);
}

#[test]
fn budget_sums_match_period_by_period_sums_across_halvings() {
    let schedule = at(0, 1_000_003);
    for (first, end) in [(0, 0), (5, 6), (E - 3, E + 4), (E - 1, 3 * E + 2), (2 * E, 2 * E + 9)] {
        let brute: u128 = (first..end).map(|period| schedule.budget(period)).sum();
        assert_eq!(schedule.budgets(first, end), brute, "{first}..{end}");
    }
}

#[test]
fn every_scheduled_budget_sums_below_the_nominal_supply_which_fits() {
    for budget in [1, 3, 1_000_000_000_000_000_000, MAX_PERIOD_BUDGET] {
        let schedule = at(0, budget);
        let nominal = schedule.nominal_supply();
        assert_eq!(nominal, 2 * u128::from(E) * budget);
        // Rounding each halving down leaves a tail below the ceiling, never above it.
        let scheduled = schedule.budgets(0, u64::MAX);
        assert!(scheduled <= nominal && nominal - scheduled < u128::from(E) * 130, "{budget}");
        assert_eq!(scheduled, (0..128).map(|h| u128::from(E) * (budget >> h)).sum::<u128>());
    }
}

#[test]
fn token_amounts_are_canonical_decimal_strings() {
    for (text, units) in [("0", 0), ("1", 1), ("340282366920938463463374607431768211455", u128::MAX)] {
        assert_eq!(text.parse::<TokenUnits>(), Ok(TokenUnits(units)));
        assert_eq!(serde_json::to_string(&TokenUnits(units)).unwrap(), format!("\"{text}\""));
        assert_eq!(serde_json::from_str::<TokenUnits>(&format!("\"{text}\"")).unwrap(), TokenUnits(units));
    }
    for bad in ["", "+1", "-1", "01", "1.0", "1e3", " 1", "340282366920938463463374607431768211456"] {
        assert!(bad.parse::<TokenUnits>().is_err(), "{bad:?}");
    }
    assert!(serde_json::from_str::<TokenUnits>("1").is_err(), "a JSON number is refused");
}

#[test]
fn the_first_halved_period_allocates_half_the_budget_through_the_engine() {
    let mut h = Harness::emitting(3);
    let [a] = funded(&mut h);
    let schedule = schedule(&h);
    // The last period at the initial budget, then the first at half of it, rounded down.
    h.advance_to(schedule.period_end(E - 2));
    round_trip(&mut h, &[(a, "1")], "100000", "99900");
    h.advance_to(schedule.period_end(E - 1));
    let crossing = h.price(common::BTC, "100000");
    assert_eq!(allocations(&crossing), [(a, E - 1, f("100"), 3)]);
    round_trip(&mut h, &[(a, "1")], "100000", "99900");
    h.advance_to(schedule.period_end(E));
    let halved = h.cmd(engine_core::command::Command::Tick);
    assert_eq!(allocations(&halved), [(a, E, f("100"), 1)]);
    assert_eq!(closed(&halved), [(E, 1, 1, 0)]);
    assert_eq!(h.engine.entitlement(a), TokenUnits(4));
    // Period 0 and the empty periods up to the last full one were cancelled on the way.
    let emissions = h.engine.emissions().unwrap();
    assert_eq!(emissions.cancelled.0, schedule.budgets(0, E - 1));
    assert_eq!(emissions.entitled.0 + emissions.cancelled.0, schedule.budgets(0, E + 1));
    assert!(cancelled_ranges(&crossing).is_empty());
}
