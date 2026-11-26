//! A measured leg's second chance (#1085): on a runner whose figures are all several times their
//! usual size a leg breaks the launch bar on unchanged code, and a regression breaks it again. A leg
//! that breaks it is run once more on the same node, and fails only if the second run breaks it too.

use std::collections::BTreeSet;

use crate::load_scenario::report::{Breach, tolerate};
use crate::load_scenario::{MISSES, Run, Window, storage};

/// What failed a measured leg's run: each budget outside the tolerance for a loaded runner both over
/// every window and over the windows the runner's storage did not cover (`storage::covered`), in which
/// it is allowed its share of misses, as the soak allows. An excuse can only pass a budget the tolerance
/// fails, never fail one it passes. Every window keeps its place, so a breach names its own window; a
/// run whose every window the storage covered, and that broke the tolerance, measured nothing. A budget
/// the storage's excuses touched, left with fewer than `MIN_JUDGED` windows to judge, is neither failed
/// nor passed (see `inconclusive`).
pub(super) fn failed(run: &Run) -> Vec<String> {
    assess(run).0
}

/// Whether `run` broke the tolerance over every window, and the storage's excuses leave too few windows
/// to say whether the bar broke: it is run again, and fails only if its second run breaks the bar with
/// enough windows measured.
pub(super) fn inconclusive(run: &Run) -> bool {
    assess(run).1
}

/// `failed` and `inconclusive` of `run`.
fn assess(run: &Run) -> (Vec<String>, bool) {
    let covered = storage::covered(&run.windows);
    let judged = covered.iter().filter(|covered| !**covered).count();
    let share = judged * MISSES / run.windows.len().max(1);
    let budgets: BTreeSet<(&str, &str)> =
        run.windows.iter().flat_map(|window| &window.breaches).map(|breach| (breach.budget, breach.quantile)).collect();
    let (mut failed, mut unsure) = (Vec::new(), false);
    for budget in budgets {
        let of_budget = |breach: &&Breach| (breach.budget, breach.quantile) == budget;
        // The budget's breaches in each window, a covered window's dropped when `excusing`.
        let breaches = |excusing: bool| -> Vec<Vec<Breach>> {
            let window = |(at, window): (usize, &Window)| {
                let dropped = excusing && covered[at];
                window.breaches.iter().filter(of_budget).filter(|_| !dropped).cloned().collect()
            };
            run.windows.iter().enumerate().map(window).collect()
        };
        if tolerate(&breaches(false), MISSES).is_empty() {
            continue;
        }
        if judged == 0 {
            return (
                vec![format!(
                    "the runner's storage stalled in or beside all {} windows, so none was judged",
                    run.windows.len()
                )],
                false,
            );
        }
        let excused = run.windows.len() - judged;
        let over = tolerate(&breaches(true), share);
        let touched = run
            .windows
            .iter()
            .zip(&covered)
            .any(|(w, covered)| *covered && w.breaches.iter().any(|breach| of_budget(&breach)));
        if !over.is_empty() && judged < storage::MIN_JUDGED && touched {
            unsure = true;
            continue;
        }
        let allowed =
            |why: String| format!("{why}, {excused} of them excused by the runner's storage, {share} allowed");
        failed.extend(over.into_iter().map(allowed));
    }
    (failed, unsure)
}

/// Whether `run`, a leg that has just ended after the `earlier` ones, is run again: a measured leg that
/// measured what it claims and broke the bar or was inconclusive, not yet run a second time. A leg that did not measure
/// what it claims fails as it is, and a ramp step breaks budgets by design.
pub(super) fn wanted(earlier: &[Run], run: &Run) -> bool {
    !run.leg.starts_with("ramp_")
        && run.gaps.is_empty()
        && (!failed(run).is_empty() || inconclusive(run))
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
    use crate::load_scenario::report::{Snapshot, judge, order_to_fill_bars};
    use crate::metrics::Metrics;

    fn unexcused(breaches: Vec<Breach>) -> Window {
        Window { breaches, excused: None }
    }

    fn run(leg: &str, over: bool) -> Run {
        let breach = Breach { budget: "order_to_fill_us", quantile: "p50", figure: 20_000.0, bar: 5_000.0 };
        let windows = vec![unexcused(if over { vec![breach] } else { Vec::new() }); 5];
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
        let at_bar = Run { windows: vec![unexcused(window(bar)); 5], ..run("launch_flow", false) };
        assert!(failed(&at_bar).is_empty(), "every window at the bar");
        let mut over = at_bar;
        over.windows[2] = unexcused(window(bar + 1));
        assert_eq!(failed(&over).len(), 1, "one window over the bar: {:?}", over.windows[2]);
    }

    /// A tail may miss its bar in two of five windows, or in its share of those the runner's storage
    /// did not excuse: one of three. A run whose every window the storage excused fails.
    #[test]
    fn a_window_the_storage_excused_is_not_judged_and_the_rest_keep_their_share_of_misses() {
        let tail = Breach { budget: "order_to_fill_us", quantile: "p99", figure: 250_000.0, bar: 36_000.0 };
        let excused = Window { breaches: vec![tail.clone()], excused: Some("the storage stalled".into()) };
        let windows = |breached: &[bool]| -> Vec<Window> {
            breached.iter().map(|over| unexcused(if *over { vec![tail.clone()] } else { Vec::new() })).collect()
        };
        let mut stalled = Run { windows: windows(&[false, false, false, true, false]), ..run("launch_flow", false) };
        (stalled.windows[0], stalled.windows[1]) = (excused.clone(), excused.clone());
        assert!(failed(&stalled).is_empty(), "one miss in three judged windows");
        stalled.windows[4] = unexcused(vec![tail.clone()]);
        let why = failed(&stalled);
        assert_eq!(why.len(), 1, "two misses in three judged windows");
        assert!(why[0].ends_with("2 of them excused by the runner's storage, 1 allowed"), "{why:?}");
        let none_excused = Run { windows: windows(&[true, true, true, false, false]), ..run("launch_flow", false) };
        assert_eq!(failed(&none_excused).len(), 1, "three misses in five windows, as before");
        let all = Run { windows: vec![excused; 5], ..run("launch_flow", false) };
        assert!(failed(&all).iter().any(|why| why.contains("none was judged")), "{:?}", failed(&all));
    }

    /// Excusing a window never fails a budget the tolerance passes over every window, and a breach in a
    /// judged window keeps its own window's number.
    #[test]
    fn an_excused_window_fails_no_run_the_tolerance_passes_and_judged_windows_keep_their_numbers() {
        let order = Breach { budget: "order_to_fill_us", quantile: "p99", figure: 250_000.0, bar: 36_000.0 };
        let frame = Breach { budget: "stream_lag_ms", quantile: "p99", figure: 200.0, bar: 144.0 };
        let excused = Window { breaches: vec![order.clone()], excused: Some("the storage stalled".into()) };
        let mut lagging = run("launch_flow", false);
        lagging.windows[0] = excused.clone();
        (lagging.windows[1], lagging.windows[2]) = (unexcused(vec![frame.clone()]), unexcused(vec![frame]));
        assert!(failed(&lagging).is_empty(), "two frame misses in five windows: {:?}", failed(&lagging));
        let median = Breach { budget: "order_to_fill_us", quantile: "p50", figure: 9_000.0, bar: 7_195.0 };
        let mut slow = run("launch_flow", false);
        (slow.windows[0], slow.windows[1], slow.windows[3]) = (excused.clone(), excused, unexcused(vec![median]));
        let why = failed(&slow);
        assert!(why.len() == 1 && why[0].starts_with("window 3:"), "{why:?}");
    }

    fn tail() -> Breach {
        Breach { budget: "order_to_fill_us", quantile: "p99", figure: 250_000.0, bar: 36_000.0 }
    }

    /// Windows in order: `E` excused by the storage, `x` over the bar in its tail, `.` clean.
    fn leg(pattern: &str) -> Run {
        let window = |mark: char| match mark {
            'E' => Window { breaches: vec![tail()], excused: Some("the storage stalled".into()) },
            'x' => unexcused(vec![tail()]),
            _ => unexcused(Vec::new()),
        };
        Run { windows: pattern.chars().map(window).collect(), ..run("launch_flow", false) }
    }

    /// Two windows left to judge of five, one over its bar, is too little to say either way: the leg
    /// is not failed on it but runs again, and a second run as short is not failed either.
    #[test]
    fn a_leg_with_two_windows_left_to_judge_is_inconclusive_and_runs_again() {
        let short = leg("x.EEE");
        assert!(failed(&short).is_empty(), "{:?}", failed(&short));
        assert!(inconclusive(&short));
        assert!(wanted(&[], &short), "runs again");
        assert!(!wanted(&[leg("x.EEE")], &short), "only once");
        assert!(judged(&[leg("x.EEE"), leg("x.EEE")]).iter().all(|run| failed(run).is_empty()));
    }

    /// With every window excused or beside one that is, nothing was measured, as before: the leg runs
    /// again and fails if the second run measures nothing either.
    #[test]
    fn a_leg_with_one_window_left_beside_the_stalls_runs_again_and_fails_if_nothing_is_measured() {
        let none = leg("EEEEx");
        assert!(wanted(&[], &none));
        assert!(failed(&none).iter().any(|why| why.contains("none was judged")));
    }

    /// Four windows measured, three over their bar: enough to judge, and past the one miss they allow.
    #[test]
    fn a_leg_with_four_windows_measured_and_over_the_bar_fails() {
        let over = leg("E.xxx");
        assert!(!inconclusive(&over));
        let why = failed(&over);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].ends_with("1 of them excused by the runner's storage, 1 allowed"), "{why:?}");
        assert!(wanted(&[], &over), "it still runs a second time");
        assert_eq!(play_legs(&[leg("E.xxx"), leg("E.xxx")]), 1);
    }

    /// The runs a leg is judged by fail as often as `failed` says; returns how many fail.
    fn play_legs(runs: &[Run]) -> usize {
        judged(runs).iter().filter(|run| !failed(run).is_empty()).count()
    }

    /// A tail over its bar in the window beside an excused one is the same stall at its edge: it is
    /// covered with it, and one with a median breach, or none, is not.
    #[test]
    fn a_stall_straddling_a_boundary_covers_its_neighbour() {
        // `.Exx.`: three misses over every window; the storage covers window 2 and leaves one in 3.
        assert!(failed(&leg(".Exx.")).is_empty(), "{:?}", failed(&leg(".Exx.")));
        // Window 3 is not beside the stall, so it is judged.
        assert_eq!(storage::covered(&leg(".Exx.").windows), [false, true, true, false, false]);
        let median = Breach { budget: "order_to_fill_us", quantile: "p50", figure: 9_000.0, bar: 7_195.0 };
        let mut slow = leg(".E...");
        slow.windows[2] = unexcused(vec![median]);
        assert_eq!(storage::covered(&slow.windows), [false, true, false, false, false]);
        assert_eq!(storage::covered(&leg("E.x.E").windows), [true, false, false, false, true]);
    }

    #[test]
    fn each_measured_leg_is_judged_by_its_last_run() {
        let runs =
            [run("launch_flow", true), run("launch_flow", false), run("ten_times", false), run("ramp_20x", true)];
        let judged: Vec<_> = judged(&runs).iter().map(|run| (run.leg.as_str(), failed(run).is_empty())).collect();
        assert_eq!(judged, [("launch_flow", true), ("ten_times", true)]);
    }
}
