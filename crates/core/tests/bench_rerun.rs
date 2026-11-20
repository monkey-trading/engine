//! `rerun_once` (#1085) on stub measurements: a guard that holds, one that breaks and then holds,
//! and one that breaks twice.

mod common;

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use common::rerun::rerun_once;

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
