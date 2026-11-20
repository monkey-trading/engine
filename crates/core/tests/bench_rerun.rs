//! `rerun_once` (#1085) on stub measurements: a guard that holds, one that breaks and then holds,
//! and one that breaks twice; and the bar scaled to the runner's speed (#1119).

mod common;

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use common::rerun::{rerun_once, scale_permille, scaled_with};

/// A stub measurement that runs `outcomes` in turn, each a figure against a bar of 500, and counts
/// its runs.
fn measuring(outcomes: &[u32], runs: &Cell<usize>) -> impl FnMut() -> u32 {
    let outcomes = outcomes.to_vec();
    move || {
        let figure = outcomes[runs.get()];
        runs.set(runs.get() + 1);
        assert!(figure <= 500, "took {figure}us against 500us");
        figure
    }
}

#[test]
fn a_first_run_within_its_bar_is_not_measured_again() {
    let runs = Cell::new(0);
    assert_eq!(rerun_once("stub", measuring(&[300, 900], &runs)), 300);
    assert_eq!(runs.get(), 1);
}

#[test]
fn a_first_run_over_its_bar_passes_when_the_second_holds() {
    let runs = Cell::new(0);
    assert_eq!(rerun_once("stub", measuring(&[900, 300], &runs)), 300);
    assert_eq!(runs.get(), 2);
}

#[test]
fn two_runs_over_the_bar_fail_with_the_second_runs_figure() {
    let runs = Cell::new(0);
    let failed = catch_unwind(AssertUnwindSafe(|| rerun_once("stub", measuring(&[900, 700, 300], &runs))))
        .expect_err("both runs broke the bar");
    assert_eq!(runs.get(), 2, "measured once more, not again");
    assert_eq!(failed.downcast_ref::<String>().map(String::as_str), Some("took 700us against 500us"));
}

/// The reference workload's time on the fast runner, and a benchmark's bar and its figure there: within
/// 30% of the bar, so a 30% regression of it is over.
const REFERENCE_US: u128 = 100_000;
const BAR_US: u128 = 500_000;
const FAST_FIGURE_US: u128 = 390_000;

/// Whether a benchmark whose figure is `figure_us` passes its bar on a runner that took `reference_us`
/// on the reference workload.
fn passes(reference_us: u128, figure_us: u128) -> bool {
    figure_us <= scaled_with(BAR_US, scale_permille(reference_us, REFERENCE_US))
}

/// A runner slower by `factor_permille` takes that much longer on both the reference workload and the
/// benchmark.
fn on_runner(factor_permille: u128) -> (u128, u128) {
    (REFERENCE_US * factor_permille / 1_000, FAST_FIGURE_US * factor_permille / 1_000)
}

#[test]
fn a_slow_runner_and_a_fast_runner_both_pass_unchanged_code() {
    for factor in [1_000, 1_400, 1_750, 2_500] {
        let (reference, figure) = on_runner(factor);
        assert!(passes(reference, figure), "unchanged code on a runner {factor} thousandths slow: {figure}us");
    }
    // The bimodal runners of #1119: 320 ms on one, 545 ms on the other, against 500 ms.
    assert!(passes(100_000, 320_000));
    assert!(passes(170_000, 545_000));
}

#[test]
fn a_slow_runner_and_a_fast_runner_both_fail_a_30_percent_regression() {
    for factor in [1_000, 1_400, 1_750, 2_500] {
        let (reference, figure) = on_runner(factor);
        let regressed = figure * 130 / 100;
        assert!(!passes(reference, regressed), "a regression on a runner {factor} thousandths slow: {regressed}us");
    }
}

#[test]
fn the_scale_stays_within_one_and_two_and_a_half() {
    // A runner faster than the reference never tightens the bar below #904's.
    assert_eq!(scale_permille(40_000, REFERENCE_US), 1_000);
    assert_eq!(scaled_with(BAR_US, scale_permille(40_000, REFERENCE_US)), BAR_US);
    // A runner slower than 2.5 times the reference is held to 2.5 times the bar.
    assert_eq!(scale_permille(900_000, REFERENCE_US), 2_500);
    assert_eq!(scaled_with(BAR_US, scale_permille(900_000, REFERENCE_US)), 1_250_000);
    assert!(!passes(900_000, 1_250_001));
    assert!(passes(900_000, 1_250_000));
    // In between it is the ratio, rounded up for a bar.
    assert_eq!(scale_permille(150_000, REFERENCE_US), 1_500);
    assert_eq!(scaled_with(1, 1_500), 2);
    // A reference of zero divides by nothing.
    assert_eq!(scale_permille(5, 0), 2_500);
}

/// Unset, the bar is the absolute one: the reference workload is not even run.
#[test]
fn a_run_without_the_variable_is_held_to_the_absolute_bar() {
    if std::env::var_os(common::rerun::SCALE_ENV).is_none() {
        assert_eq!(common::rerun::runner_permille(), 1_000);
        assert_eq!(common::rerun::scaled(500_000), 500_000);
    }
}
