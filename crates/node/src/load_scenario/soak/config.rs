//! The soak's length and cadences, from the environment's variables.

use std::time::Duration;

use crate::load_scenario::generator::traders::SESSION_LIFE_MS;
use crate::load_scenario::plan::Leg;

/// The soak's cadences and length.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Config {
    pub(super) total: Duration,
    pub(super) report: Duration,
    pub(super) fault_every: Duration,
    pub(super) restart_every: Option<Duration>,
    pub(super) window: Duration,
    pub(super) snapshot: Duration,
    pub(super) multiple: f64,
    /// A run that tests the harness (`SOAK_SHORT=1`): its events come too close together to leave a quiet
    /// window and its intervals too short for memory to settle, so neither fails an interval.
    pub(super) short: bool,
    /// Alerts the run raises however it goes, each a known issue it does not fail on (`SOAK_KNOWN_ALERTS`,
    /// comma-separated names).
    pub(super) known_alerts: Vec<String>,
}

impl Config {
    /// The configuration `get` names, from the environment's variables: the proposal's 8 hours at ten
    /// times launch flow, a fault every 20 minutes, a restart every 2 hours (#909, #904).
    pub(super) fn from(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let number = |name: &str, default: f64| {
            get(name).map_or(default, |value| value.parse().unwrap_or_else(|_| panic!("{name} is a number")))
        };
        let secs = |name: &str, default: f64, unit: f64| {
            let value = number(name, default);
            assert!(value >= 0.0 && value.is_finite(), "{name} is not negative");
            Duration::from_secs_f64(value * unit)
        };
        let restart_every = secs("SOAK_RESTART_HOURS", 2.0, 3_600.0);
        let config = Config {
            total: secs("SOAK_HOURS", 8.0, 3_600.0),
            report: secs("SOAK_REPORT_MINUTES", 60.0, 60.0),
            fault_every: secs("SOAK_FAULT_MINUTES", 20.0, 60.0),
            restart_every: (!restart_every.is_zero()).then_some(restart_every),
            window: secs("SOAK_WINDOW_SECS", 120.0, 1.0),
            snapshot: secs("SOAK_SNAPSHOT_SECS", 300.0, 1.0),
            multiple: number("SOAK_MULTIPLE", 10.0),
            short: get("SOAK_SHORT").is_some_and(|value| value == "1"),
            known_alerts: get("SOAK_KNOWN_ALERTS")
                .map(|names| names.split(',').filter(|name| !name.is_empty()).map(str::to_owned).collect())
                .unwrap_or_default(),
        };
        assert!(!config.total.is_zero() && !config.window.is_zero(), "SOAK_HOURS and SOAK_WINDOW_SECS are above 0");
        assert!(config.report >= config.window, "SOAK_REPORT_MINUTES holds at least one window");
        // The traders' session keys are registered once, for `SESSION_LIFE_MS`.
        let outlived = Duration::from_millis(SESSION_LIFE_MS as u64).saturating_sub(Duration::from_secs(3_600));
        assert!(config.total < outlived, "SOAK_HOURS is under the traders' session keys' life less an hour");
        assert!(!config.fault_every.is_zero() && config.multiple > 0.0, "a fault cadence and a multiple above 0");
        config
    }

    /// The intervals the run's length takes, each a leg at the soak's multiple of launch flow: whole
    /// report intervals, then one for the rest if it holds a window.
    pub(super) fn legs(&self) -> Vec<Leg> {
        let (total, report) = (self.total.as_secs(), self.report.as_secs().max(1));
        let mut lengths = vec![report; (total / report) as usize];
        if total % report >= self.window.as_secs().max(1) || lengths.is_empty() {
            lengths.push((total % report).max(1));
        }
        let leg = |(at, secs): (usize, u64)| Leg {
            name: format!("interval_{}", at + 1),
            multiple: self.multiple,
            secs,
            windows: self.windows_in(secs),
        };
        lengths.into_iter().enumerate().map(leg).collect()
    }

    /// The windows an interval of `secs` is judged in.
    pub(super) fn windows_in(&self, secs: u64) -> u32 {
        (secs as f64 / self.window.as_secs_f64()).round().max(1.0) as u32
    }

    /// How often resident memory and the stats worker's lag are sampled.
    pub(super) fn sample_every(&self) -> Duration {
        (self.report / 10).clamp(Duration::from_secs(5), Duration::from_secs(60))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn config(vars: &[(&str, &str)]) -> Config {
        let vars: BTreeMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from(&|name| vars.get(name).cloned())
    }

    #[test]
    fn the_defaults_are_the_proposals_eight_hours_at_ten_times_launch_flow() {
        let config = config(&[]);
        assert_eq!(config.total, Duration::from_secs(8 * 3_600));
        assert_eq!(config.fault_every, Duration::from_secs(20 * 60));
        assert_eq!(config.restart_every, Some(Duration::from_secs(2 * 3_600)));
        assert_eq!(config.multiple, 10.0);
        assert!(!config.short, "a full run bounds memory and wants a quiet window in every interval");
        let legs = config.legs();
        assert_eq!(legs.len(), 8, "an interval an hour");
        assert!(legs.iter().all(|leg| leg.secs == 3_600 && leg.multiple == 10.0));
        assert!(legs.iter().all(|leg| leg.windows == 30), "windows of two minutes");
    }

    #[test]
    fn the_variables_set_the_length_and_the_cadences_and_zero_restarts_means_none() {
        let config = config(&[
            ("SOAK_HOURS", "0.15"),
            ("SOAK_REPORT_MINUTES", "3"),
            ("SOAK_FAULT_MINUTES", "1"),
            ("SOAK_RESTART_HOURS", "0"),
            ("SOAK_WINDOW_SECS", "20"),
        ]);
        assert_eq!(config.legs().len(), 3, "nine minutes in intervals of three");
        assert_eq!(config.fault_every, Duration::from_secs(60));
        assert_eq!(config.restart_every, None);
        assert!(config.legs().iter().all(|leg| leg.windows == 9));
        assert!(self::config(&[("SOAK_SHORT", "1")]).short);
        let partial = self::config(&[("SOAK_HOURS", "2.5")]);
        let secs: Vec<u64> = partial.legs().iter().map(|leg| leg.secs).collect();
        assert_eq!(secs, [3_600, 3_600, 1_800], "the last interval is the part hour, not a whole one");
        let windows: Vec<u32> = partial.legs().iter().map(|leg| leg.windows).collect();
        assert_eq!(windows, [30, 30, 15], "each interval holds windows of two minutes");
        let tiny = self::config(&[("SOAK_HOURS", "0.5"), ("SOAK_REPORT_MINUTES", "60")]);
        assert_eq!(tiny.legs().iter().map(|leg| leg.secs).collect::<Vec<_>>(), [1_800], "a run under one interval");
        let names = self::config(&[("SOAK_KNOWN_ALERTS", "a,b")]).known_alerts;
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    #[should_panic(expected = "session keys' life")]
    fn a_run_the_session_keys_would_not_outlast_is_refused() {
        config(&[("SOAK_HOURS", "12")]);
    }

    #[test]
    #[should_panic(expected = "SOAK_REPORT_MINUTES holds at least one window")]
    fn an_interval_shorter_than_a_window_is_refused() {
        config(&[("SOAK_REPORT_MINUTES", "1"), ("SOAK_WINDOW_SECS", "120")]);
    }

    #[test]
    fn sampling_is_a_tenth_of_an_interval_between_five_and_sixty_seconds() {
        assert_eq!(config(&[]).sample_every(), Duration::from_secs(60));
        let short = config(&[("SOAK_REPORT_MINUTES", "3"), ("SOAK_WINDOW_SECS", "20")]);
        assert_eq!(short.sample_every(), Duration::from_secs(18));
    }
}
