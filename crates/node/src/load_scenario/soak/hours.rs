//! Each report interval's figures, judged against the pass bounds, and printed: the windows inside
//! #904's launch bar outside the injected faults, memory that grows no more than the state does, every
//! market priced again after each fault, no alert but the faults' and no restart slower than the
//! recovery bar. The interval is an hour in the full soak.

use std::path::Path;
use std::time::Duration;

use crate::load_scenario::report::{Breach, tolerate};
use crate::load_scenario::soak::faults::Done;
use crate::load_scenario::{MISSES, WINDOWS};

/// Resident memory may grow this much, as a share of the first interval's, beyond what the state's
/// growth explains.
pub(super) const MEMORY_GROWTH: f64 = 0.10;
/// #904's bar for a restart from the newest cache to its first command.
pub(super) const RESTART_BAR: Duration = Duration::from_secs(2 * 60);

/// A window of a leg: the launch bar's breaches and the gaps in what it measured, and what excuses it
/// from the bar, if anything.
#[derive(Clone, Debug)]
pub(super) struct Win {
    pub index: u32,
    pub breaches: Vec<Breach>,
    pub gaps: Vec<String>,
    pub excused: Option<String>,
}

/// A restart: how long the node took to recover from its newest cache, and to price every market.
#[derive(Clone, Debug)]
pub(super) struct Restarted {
    /// Whether the node was dropped as a crash does, with no drain and no final checkpoint.
    pub abrupt: bool,
    pub stopped: Duration,
    pub recovered: Duration,
    pub priced: Result<Duration, String>,
}

/// What the node held at a point of the run: its resident memory, and the size of the newest cache,
/// which the state's size sets.
#[derive(Clone, Copy, Debug)]
pub(super) struct Standing {
    pub rss_mib: u64,
    pub state_bytes: u64,
}

/// The bytes the run holds on disk.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Disk {
    pub log: u64,
    /// The recovery caches and the history index they pair with.
    pub snapshots: u64,
}

/// One interval of the run.
#[derive(Clone, Debug, Default)]
pub(super) struct Interval {
    pub windows: Vec<Win>,
    pub faults: Vec<Done>,
    pub restarts: Vec<Restarted>,
    /// An alert no event excuses, by name.
    pub unexpected_alerts: Vec<String>,
    /// A fault that must raise an alert and did not, by the fault's name.
    pub missing_alerts: Vec<String>,
}

/// What fails `interval` against the bounds that need nothing from the intervals before it. An interval
/// with every window excused fails, unless `judged_required` is off, as for a run whose faults come
/// too close together to leave a quiet window.
pub(super) fn failures(interval: &Interval, judged_required: bool) -> Vec<String> {
    let clean: Vec<&Win> = interval.windows.iter().filter(|window| window.excused.is_none()).collect();
    let mut failed = Vec::new();
    if clean.is_empty() && judged_required {
        failed.push("no window outside the faults and restarts to judge".to_owned());
    }
    // Every window keeps its place, so a breach names its own window; an excused one holds none.
    let breaches: Vec<Vec<Breach>> = (interval.windows.iter())
        .map(|window| if window.excused.is_none() { window.breaches.clone() } else { Vec::new() })
        .collect();
    failed.extend(tolerate(&breaches, clean.len() * MISSES / WINDOWS as usize));
    for window in &clean {
        failed.extend(window.gaps.iter().map(|gap| format!("window {} measured nothing right: {gap}", window.index)));
    }
    for done in &interval.faults {
        if let Err(why) = &done.priced_again {
            failed.push(format!("{}: {why}", done.fault.name()));
        }
    }
    for restart in &interval.restarts {
        if restart.recovered > RESTART_BAR {
            let how = if restart.abrupt { "crashed" } else { "drained" };
            failed.push(format!(
                "a restart that {how} in {:?} took {:?} to recover, over {RESTART_BAR:?}",
                restart.stopped, restart.recovered
            ));
        }
        if let Err(why) = &restart.priced {
            failed.push(format!("after a restart: {why}"));
        }
    }
    failed.extend(interval.unexpected_alerts.iter().map(|name| format!("alert {name} raised outside any fault")));
    failed.extend(interval.missing_alerts.iter().map(|name| format!("{name} raised no alert")));
    failed
}

/// What fails memory at `now` against the first interval's `first`: resident memory more than
/// `MEMORY_GROWTH` over it, plus the share the newest cache's size has grown by, which tracks the
/// state's.
pub(super) fn memory_failure(first: Standing, now: Standing) -> Option<String> {
    let state_growth = (now.state_bytes as f64 / first.state_bytes.max(1) as f64 - 1.0).max(0.0);
    let bound = first.rss_mib as f64 * (1.0 + MEMORY_GROWTH + state_growth);
    (now.rss_mib as f64 > bound).then(|| {
        format!(
            "resident memory {} MiB over {bound:.0} MiB: the first interval's {} MiB, {:.0}% more, \
             and the state's growth of {:.0}%",
            now.rss_mib,
            first.rss_mib,
            MEMORY_GROWTH * 100.0,
            state_growth * 100.0
        )
    })
}

/// The first interval's memory baseline once an interval has a cache: the state's size is what the bound
/// scales by, so an interval with none sets none and the bound waits for the first that has.
pub(super) fn baseline(first: Option<Standing>, now: Standing) -> Option<Standing> {
    first.or((now.state_bytes > 0).then_some(now))
}

/// The median of `samples`, 0 with none.
pub(super) fn median(samples: &[u64]) -> u64 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied().unwrap_or(0)
}

/// The bytes in the files under `path`, 0 for a directory that is not there.
pub(super) fn dir_bytes(path: &Path) -> u64 {
    let Ok(listing) = std::fs::read_dir(path) else { return 0 };
    listing
        .filter_map(Result::ok)
        .map(|item| match item.file_type() {
            Ok(kind) if kind.is_dir() => dir_bytes(&item.path()),
            Ok(_) => item.metadata().map_or(0, |found| found.len()),
            Err(_) => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use tokio::time::Instant;

    use super::*;
    use crate::load_scenario::soak::faults::Fault;

    fn window(index: u32, breaches: Vec<Breach>, excused: Option<&str>) -> Win {
        Win { index, breaches, gaps: Vec::new(), excused: excused.map(str::to_owned) }
    }

    fn stamp_p50() -> Breach {
        Breach { budget: "price_to_stamp_us", quantile: "p50", figure: 2_000.0, bar: 1_000.0 }
    }

    fn done(fault: Fault, priced_again: Result<Duration, String>) -> Done {
        Done { fault, began: Instant::now(), priced_again }
    }

    #[test]
    fn a_breach_in_a_window_a_fault_excuses_is_not_held_against_the_interval() {
        let interval = Interval {
            windows: vec![
                window(0, vec![], None),
                window(1, vec![stamp_p50()], Some("stalled")),
                window(2, vec![], None),
            ],
            ..Interval::default()
        };
        assert!(failures(&interval, true).is_empty(), "{:?}", failures(&interval, true));
        let mut over = interval.clone();
        over.windows[2].breaches = vec![stamp_p50()];
        assert_eq!(failures(&over, true).len(), 1, "the same breach in a window no fault excuses fails the median");
    }

    #[test]
    fn a_median_breach_names_its_own_window_after_excused_ones() {
        let interval = Interval {
            windows: vec![
                window(0, vec![], Some("stalled")),
                window(1, vec![], Some("stalled")),
                window(2, vec![stamp_p50()], None),
            ],
            ..Interval::default()
        };
        let failed = failures(&interval, true);
        assert!(failed.len() == 1 && failed[0].starts_with("window 2:"), "{failed:?}");
    }

    #[test]
    fn an_interval_with_every_window_excused_has_nothing_to_judge_and_fails() {
        let interval = Interval { windows: vec![window(0, vec![], Some("restart"))], ..Interval::default() };
        assert_eq!(failures(&interval, true), ["no window outside the faults and restarts to judge"]);
    }

    #[test]
    fn an_interval_with_nothing_to_judge_passes_when_the_run_allows_it() {
        let interval = Interval { windows: vec![window(0, vec![], Some("restart"))], ..Interval::default() };
        assert!(failures(&interval, false).is_empty());
        let failing = Interval { faults: vec![done(Fault::Quiet, Err("unpriced".into()))], ..interval };
        assert_eq!(failures(&failing, false).len(), 1, "everything else is still held");
    }

    #[test]
    fn a_gap_in_what_a_clean_window_measured_fails_and_one_in_an_excused_window_does_not() {
        let gap = |excused| Win { gaps: vec!["3 requests refused".to_owned()], ..window(4, vec![], excused) };
        let clean = Interval { windows: vec![window(0, vec![], None), gap(None)], ..Interval::default() };
        assert_eq!(failures(&clean, true), ["window 4 measured nothing right: 3 requests refused"]);
        let excused = Interval { windows: vec![window(0, vec![], None), gap(Some("dropped"))], ..Interval::default() };
        assert!(failures(&excused, true).is_empty());
    }

    #[test]
    fn a_fault_that_left_a_market_unpriced_a_slow_restart_and_a_stray_alert_each_fail() {
        let interval = Interval {
            windows: vec![window(0, vec![], None)],
            faults: vec![done(Fault::Stalled, Ok(Duration::from_secs(1))), done(Fault::Quiet, Err("unpriced".into()))],
            restarts: vec![Restarted {
                abrupt: true,
                stopped: Duration::ZERO,
                recovered: RESTART_BAR * 2,
                priced: Ok(Duration::ZERO),
            }],
            unexpected_alerts: vec!["log_sync_held".to_owned()],
            missing_alerts: vec!["slow_link".to_owned()],
        };
        let failed = failures(&interval, true);
        assert_eq!(failed.len(), 4, "{failed:?}");
        assert!(failed.iter().any(|why| why.starts_with("quiet_product: ")));
        assert!(failed.iter().any(|why| why.contains("a restart that crashed")));
        assert!(failed.iter().any(|why| why.contains("log_sync_held")));
        assert!(failed.iter().any(|why| why.contains("slow_link raised no alert")));
    }

    #[test]
    fn memory_may_grow_a_tenth_and_as_much_as_the_state_does() {
        let first = Standing { rss_mib: 1_000, state_bytes: 100 };
        let at = |rss_mib, state_bytes| memory_failure(first, Standing { rss_mib, state_bytes });
        assert!(at(1_100, 100).is_none(), "a tenth over, state unchanged");
        assert!(at(1_101, 100).is_some(), "past a tenth");
        assert!(at(1_290, 120).is_none(), "a tenth and the state's fifth");
        assert!(at(1_310, 120).is_some(), "past both");
        assert!(at(900, 50).is_none(), "a state that shrank gives no extra room, and less memory is fine");
        assert!(at(1_101, 50).is_some(), "a state that shrank gives no extra room");
    }

    #[test]
    fn the_memory_baseline_waits_for_an_interval_with_a_cache_and_then_stays() {
        let (empty, cached) = (Standing { rss_mib: 100, state_bytes: 0 }, Standing { rss_mib: 120, state_bytes: 50 });
        assert!(baseline(None, empty).is_none(), "no cache, no baseline");
        assert_eq!(baseline(None, cached).map(|first| first.rss_mib), Some(120));
        let later = Standing { rss_mib: 500, state_bytes: 80 };
        assert_eq!(baseline(Some(cached), later).map(|first| first.rss_mib), Some(120), "it is not retaken");
    }

    #[test]
    fn the_median_and_a_directorys_bytes_are_read() {
        assert_eq!(median(&[5, 1, 9]), 5);
        assert_eq!(median(&[]), 0);
        let dir = crate::test_support::scratch_dir("soak-hours");
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(dir.join("a"), [0u8; 10]).unwrap();
        std::fs::write(dir.join("inner/b"), [0u8; 32]).unwrap();
        assert_eq!(dir_bytes(&dir), 42);
        assert_eq!(dir_bytes(&dir.join("missing")), 0);
    }
}
