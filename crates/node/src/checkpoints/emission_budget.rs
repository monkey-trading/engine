//! The vault's MONKEY cap against the engine's emission schedule. The vault derives the cap from the
//! period budget it was deployed with, and the operator signs the engine's budget separately; a
//! mismatch would strand entitlements past the cap, and neither value can change once set. The
//! engine cannot read the chain, so the node compares them: before it sequences an initialisation,
//! and when it opens a log that records one.

use alloy::primitives::U256;
use anyhow::{Context, ensure};
use engine_core::emissions::{Emissions, MAX_PERIOD_BUDGET, PERIODS_PER_HALVING, TokenUnits};

/// The period budget a MONKEY cap was derived from: the cap divided by `2 · PERIODS_PER_HALVING`.
/// Error cases:
/// - a cap that is not `2 · PERIODS_PER_HALVING` times a budget from 1 to `MAX_PERIOD_BUDGET`
pub(crate) fn budget_from_cap(cap: U256) -> anyhow::Result<TokenUnits> {
    let per_budget = U256::from(2 * PERIODS_PER_HALVING);
    ensure!(cap % per_budget == U256::ZERO, "MONKEY cap {cap} is not a whole number of period budgets");
    let budget: u128 = (cap / per_budget).try_into().context("MONKEY cap is past every period budget")?;
    ensure!((1..=MAX_PERIOD_BUDGET).contains(&budget), "MONKEY cap {cap} gives period budget {budget}, out of range");
    Ok(TokenUnits(budget))
}

/// Refuses an emission period budget other than the vault's.
/// Error cases:
/// - `budget` differs from `vault`
pub(crate) fn check_budget(vault: TokenUnits, budget: TokenUnits) -> Result<(), String> {
    if budget == vault {
        return Ok(());
    }
    Err(format!("period budget {budget} differs from the vault's {vault}, which sets MONKEY's cap"))
}

/// Refuses a log whose recorded emission schedule has another period budget than the vault's.
/// Error cases:
/// - the log records emissions with a period budget other than `vault`
pub(crate) fn check_recorded(vault: TokenUnits, emissions: Option<&Emissions>) -> anyhow::Result<()> {
    match emissions {
        Some(emissions) => check_budget(vault, emissions.schedule.period_budget).map_err(anyhow::Error::msg),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use engine_core::emissions::EmissionSchedule;

    use super::*;

    #[test]
    fn a_cap_gives_back_the_budget_it_was_derived_from() {
        for budget in [1, 1_000_000_000_000_000_000, MAX_PERIOD_BUDGET] {
            let cap = U256::from(2 * PERIODS_PER_HALVING) * U256::from(budget);
            assert_eq!(budget_from_cap(cap).unwrap(), TokenUnits(budget));
        }
        let cap = |budget: u128| U256::from(2 * PERIODS_PER_HALVING) * U256::from(budget);
        for bad in [U256::ZERO, cap(5) + U256::from(1), cap(MAX_PERIOD_BUDGET + 1), U256::MAX] {
            assert!(budget_from_cap(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_the_vaults_budget_passes() {
        assert_eq!(check_budget(TokenUnits(7), TokenUnits(7)), Ok(()));
        let refused = check_budget(TokenUnits(7), TokenUnits(70)).unwrap_err();
        assert!(refused.contains("70 differs from the vault's 7"), "{refused}");

        let emissions = |budget| Emissions {
            schedule: EmissionSchedule { start_ms: 0, period_budget: TokenUnits(budget) },
            period: 0,
            entitled: TokenUnits(0),
            cancelled: TokenUnits(0),
        };
        assert!(check_recorded(TokenUnits(7), None).is_ok());
        assert!(check_recorded(TokenUnits(7), Some(&emissions(7))).is_ok());
        assert!(check_recorded(TokenUnits(7), Some(&emissions(8))).is_err());
    }
}
