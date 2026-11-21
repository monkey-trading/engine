//! The alerts the node raises over the run, kept with when they were raised: a fault's own alert is
//! looked for from where the fault began, however many interval reports have come since, and an
//! interval's report judges only the alerts raised since the last, each by whether an event that
//! explains its name was under way.

use std::collections::BTreeMap;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::load_scenario::soak::timeline::{ALERT_SETTLE, Timeline};
use crate::test_support::Alert;

/// The alerts raised, in the order they arrived.
pub(super) struct Alerts {
    rx: mpsc::UnboundedReceiver<Alert>,
    raised: Vec<(Instant, String)>,
    /// How many of `raised` an interval's report has judged.
    reported: usize,
    /// The alerts raised since the last report, by name.
    counts: BTreeMap<String, u32>,
    /// Alerts the run is told to expect whatever it does, by name, each a known issue.
    known: Vec<String>,
}

impl Alerts {
    pub(super) fn new(rx: mpsc::UnboundedReceiver<Alert>, known: Vec<String>) -> Self {
        Alerts { rx, raised: Vec::new(), reported: 0, counts: BTreeMap::new(), known }
    }

    /// The next alert the node raises.
    pub(super) async fn recv(&mut self) -> Option<Alert> {
        self.rx.recv().await
    }

    /// Records `alert` as raised now. The first of each name since the last report is printed, and
    /// the report counts the rest.
    pub(super) fn note(&mut self, alert: &Alert) {
        let name = alert.get("alert").cloned().unwrap_or_default();
        let count = self.counts.entry(name.clone()).or_default();
        *count += 1;
        if *count == 1 {
            println!("SOAK alert {name}: {alert:?}");
        }
        self.raised.push((Instant::now(), name));
    }

    /// Records every alert already waiting, so each is stamped before what follows it.
    pub(super) fn drain(&mut self) {
        while let Ok(alert) = self.rx.try_recv() {
            self.note(&alert);
        }
    }

    /// Whether an alert named `name` was raised at or after `from`.
    pub(super) fn raised_since(&self, from: Instant, name: &str) -> bool {
        self.raised.iter().any(|(at, raised)| *at >= from && raised == name)
    }

    /// The alerts raised since the last report that neither a known issue nor an event under way when
    /// they were raised explains, by name; and the counts of every alert raised since it.
    pub(super) fn report(&mut self, timeline: &Timeline) -> (Vec<String>, BTreeMap<String, u32>) {
        let unexplained = self.raised[self.reported..]
            .iter()
            .filter(|(at, name)| !self.known.contains(name) && !timeline.explained(*at, name, ALERT_SETTLE))
            .map(|(_, name)| name.clone())
            .collect();
        self.reported = self.raised.len();
        (unexplained, std::mem::take(&mut self.counts))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::load_scenario::soak::timeline::Timeline;

    fn alert(name: &str) -> Alert {
        Alert::from([("alert".to_owned(), name.to_owned())])
    }

    #[test]
    fn a_report_judges_only_the_alerts_raised_since_the_last_and_a_known_issue_is_never_judged() {
        let (_tx, rx) = mpsc::unbounded_channel();
        let mut alerts = Alerts::new(rx, vec!["snapshot_write_failed".to_owned()]);
        let mut timeline = Timeline::default();
        let started = Instant::now();
        let place = timeline.begin_at("stalled", started, &["market_no_price"]);
        alerts.note(&alert("market_no_price"));
        alerts.note(&alert("log_sync_held"));
        alerts.note(&alert("snapshot_write_failed"));
        timeline.end(place);
        let (unexplained, counts) = alerts.report(&timeline);
        assert_eq!(unexplained, ["log_sync_held"], "the fault explains its own alert and nothing else");
        assert_eq!(counts.len(), 3);
        let (again, counts) = alerts.report(&timeline);
        assert!(again.is_empty() && counts.is_empty(), "an alert is judged in one report only");
        assert!(alerts.raised_since(started, "market_no_price"), "its history outlives the report");
        assert!(!alerts.raised_since(Instant::now() + Duration::from_secs(1), "market_no_price"));
    }
}
