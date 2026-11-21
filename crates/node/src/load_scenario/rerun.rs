//! A measured leg's second chance (#1085): on a runner whose figures are all several times their
//! usual size a leg breaks the launch bar on unchanged code, and a regression breaks it again. A leg
//! that breaks it is run once more on the same node, and fails only if the second run breaks it too.

use crate::load_scenario::report::tolerate;
use crate::load_scenario::{MISSES, Run};

/// What failed a measured leg's run: the bar's breaches outside the tolerance for a loaded runner.
pub(super) fn failed(run: &Run) -> Vec<String> {
    tolerate(&run.windows, MISSES)
}

/// Whether `run`, a leg that has just ended after the `earlier` ones, is run again: a measured leg that
/// measured what it claims and broke the bar, not yet run a second time. A leg that did not measure
/// what it claims fails as it is, and a ramp step breaks budgets by design.
pub(super) fn wanted(earlier: &[Run], run: &Run) -> bool {
    !run.leg.starts_with("ramp_")
        && run.gaps.is_empty()
        && !failed(run).is_empty()
        && !earlier.iter().any(|before| before.leg == run.leg)
}

/// The run each measured leg is judged by: its last.
pub(super) fn judged(runs: &[Run]) -> Vec<&Run> {
    let last = |(at, run): &(usize, &Run)| runs[at + 1..].iter().all(|later| later.leg != run.leg);
    runs.iter().enumerate().filter(|(_, run)| !run.leg.starts_with("ramp_")).filter(last).map(|(_, run)| run).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_scenario::plan::{Figures, Quantiles};
    use crate::load_scenario::report::{Breach, Snapshot, judge, order_to_fill_bars};
    use crate::metrics::Metrics;

    fn run(leg: &str, over: bool) -> Run {
        let breach = Breach { budget: "order_to_fill_us", quantile: "p50", figure: 20_000.0, bar: 5_000.0 };
        let windows = vec![if over { vec![breach] } else { Vec::new() }; 5];
        Run { leg: leg.into(), multiple: 1.0, breaches: Vec::new(), windows, gaps: Vec::new() }
    }

    /// Plays a leg's runs, `outcomes` in turn (true: over the bar), as `drive` answers each one, and
    /// returns how many ran and whether the leg failed.
    fn play(outcomes: &[bool]) -> (usize, bool) {
        let mut runs = Vec::new();
        loop {
            let next = run("launch_flow", outcomes[runs.len()]);
            let again = wanted(&runs, &next);
            runs.push(next);
            if !again {
                break;
            }
        }
        let failed = judged(&runs).iter().any(|run| !failed(run).is_empty());
        (runs.len(), failed)
    }

    #[test]
    fn a_leg_within_the_bar_runs_once() {
        assert_eq!(play(&[false, true]), (1, false));
    }

    #[test]
    fn a_leg_over_the_bar_that_then_holds_it_passes_on_its_second_run() {
        assert_eq!(play(&[true, false, true]), (2, false));
    }

    #[test]
    fn a_leg_over_the_bar_twice_fails_after_its_second_run() {
        assert_eq!(play(&[true, true, false]), (2, true));
    }

    #[test]
    fn a_leg_that_did_not_measure_what_it_claims_and_a_ramp_step_are_not_run_again() {
        let mut unmeasured = run("launch_flow", true);
        unmeasured.gaps = vec!["no market order filled".into()];
        assert!(!wanted(&[], &unmeasured));
        assert!(!wanted(&[], &run("ramp_20x", true)));
    }

    /// Order-to-fill's median is held to the launch bar's 5 ms, at this runner's scale, in every window:
    /// a run at the bar in every window holds, and one window just over it fails the run.
    #[test]
    fn a_run_whose_order_to_fill_median_passes_its_bar_in_one_window_fails() {
        let metrics = Metrics::new();
        let before = Snapshot::take(&metrics);
        metrics.latency.price_to_stamp.observe(0.000_1);
        let node = Snapshot::take(&metrics).since(&before);
        let bar = order_to_fill_bars()[0];
        let window = |p50| {
            let filled = Quantiles { count: 100, p50, p99: p50, p999: p50, max: p50 };
            judge(&Figures { order_to_fill_us: filled, ..Figures::default() }, &node)
        };
        let at_bar = Run { windows: vec![window(bar); 5], ..run("launch_flow", false) };
        assert!(failed(&at_bar).is_empty(), "every window at the bar");
        let mut over = at_bar;
        over.windows[2] = window(bar + 1);
        assert_eq!(failed(&over).len(), 1, "one window over the bar: {:?}", over.windows[2]);
    }

    #[test]
    fn each_measured_leg_is_judged_by_its_last_run() {
        let runs =
            [run("launch_flow", true), run("launch_flow", false), run("ten_times", false), run("ramp_20x", true)];
        let judged: Vec<_> = judged(&runs).iter().map(|run| (run.leg.as_str(), failed(run).is_empty())).collect();
        assert_eq!(judged, [("launch_flow", true), ("ten_times", true)]);
    }
}
