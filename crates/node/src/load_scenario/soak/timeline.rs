//! What the soak has done to the node, and when, so a window of the run or an alert that falls in one
//! is excused only by something the soak did: an injected fault, a restart, or the start.

use std::time::Duration;

use tokio::time::Instant;

/// How long after an event a window is still excused: the queue it left drains.
pub(super) const WINDOW_SETTLE: Duration = Duration::from_secs(30);
/// How long after an event an alert is still excused: a market priced again is only seen to be by the
/// monitor's next check.
pub(super) const ALERT_SETTLE: Duration = Duration::from_secs(60);

/// The alerts a feed fault, a restart or the start may raise: a market with a source short or a venue
/// down, which every one of them can cause.
pub(super) const FEED_ALERTS: [&str; 5] =
    ["market_no_price", "venue_symbol_silent", "venue_feed_down", "venue_delayed", "venue_stale"];

/// The alerts the start may raise: the book's house loss, the RWA stand-ins' contracts the markets do not
/// take, and a feed still connecting.
pub(super) fn start_alerts() -> Vec<&'static str> {
    [&FEED_ALERTS[..], &["house_deficit", "house_loss_stress", "prices_rejected"]].concat()
}

/// The alerts a restart may raise: a feed reconnecting, the stand-ins' contracts again, and the house's
/// loss, which a new process pages again while it stands.
pub(super) fn restart_alerts() -> Vec<&'static str> {
    [&FEED_ALERTS[..], &["house_deficit", "house_loss_stress", "prices_rejected"]].concat()
}

/// One thing the soak did to the node, over `from` to `to`, which is `None` while it goes on, and the
/// alerts it explains.
struct Event {
    name: String,
    explains: Vec<String>,
    from: Instant,
    to: Option<Instant>,
}

/// The events of a run, by when they happened.
#[derive(Default)]
pub(super) struct Timeline {
    events: Vec<Event>,
}

impl Timeline {
    /// Records that `name` began now, explaining the alerts `explains`, and returns its place for `end`.
    pub(super) fn begin(&mut self, name: &str, explains: &[&str]) -> usize {
        self.begin_at(name, Instant::now(), explains)
    }

    /// Records that `name` began at `from`, which has passed, and returns its place for `end`.
    pub(super) fn begin_at(&mut self, name: &str, from: Instant, explains: &[&str]) -> usize {
        let explains = explains.iter().map(|alert| (*alert).to_owned()).collect();
        self.events.push(Event { name: name.to_owned(), explains, from, to: None });
        self.events.len() - 1
    }

    /// Records that the event at `place` ended now.
    pub(super) fn end(&mut self, place: usize) {
        self.events[place].to = Some(Instant::now());
    }

    /// Whether an event that explains the alert `name` was under way at `at`, or ended no more than
    /// `settle` before it.
    pub(super) fn explained(&self, at: Instant, name: &str, settle: Duration) -> bool {
        self.events.iter().any(|event| {
            event.from <= at
                && event.to.is_none_or(|ended| ended + settle >= at)
                && event.explains.iter().any(|explained| explained == name)
        })
    }

    /// The first event that overlaps `from` to `to`, or ended no more than `settle` before `from`.
    pub(super) fn excuse(&self, from: Instant, to: Instant, settle: Duration) -> Option<&str> {
        self.events
            .iter()
            .find(|event| event.from <= to && event.to.is_none_or(|ended| ended + settle >= from))
            .map(|event| event.name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_is_excused_by_an_event_it_overlaps_or_follows_within_the_settle_time() {
        let origin = Instant::now();
        let at = |secs| origin + Duration::from_secs(secs);
        let mut timeline = Timeline::default();
        let place = timeline.begin_at("stalled", at(100), &[]);
        timeline.events[place].to = Some(at(120));
        let excused = |from, to| timeline.excuse(at(from), at(to), WINDOW_SETTLE);
        assert_eq!(excused(0, 99), None, "a window ending before the event");
        assert_eq!(excused(90, 110), Some("stalled"), "a window the event begins in");
        assert_eq!(excused(105, 115), Some("stalled"), "a window inside the event");
        assert_eq!(excused(125, 140), Some("stalled"), "a window beginning within the settle time");
        assert_eq!(excused(151, 170), None, "a window beginning after the settle time");
    }

    #[test]
    fn an_event_still_going_excuses_every_later_window() {
        let origin = Instant::now();
        let mut timeline = Timeline::default();
        let place = timeline.begin("restart", &["market_no_price"]);
        let later = origin + Duration::from_secs(3_600);
        assert_eq!(timeline.excuse(later, later, ALERT_SETTLE), Some("restart"));
        timeline.end(place);
        assert_eq!(timeline.excuse(later + ALERT_SETTLE * 2, later + ALERT_SETTLE * 3, ALERT_SETTLE), None);
    }

    #[test]
    fn an_alert_is_explained_only_by_an_event_that_names_it_and_was_under_way() {
        let origin = Instant::now();
        let at = |secs| origin + Duration::from_secs(secs);
        let mut timeline = Timeline::default();
        let place = timeline.begin_at("dropped", at(10), &["venue_feed_down"]);
        timeline.events[place].to = Some(at(20));
        let explained = |secs, name| timeline.explained(at(secs), name, ALERT_SETTLE);
        assert!(explained(15, "venue_feed_down"));
        assert!(!explained(15, "snapshot_write_failed"), "a fault does not explain another alert");
        assert!(!explained(5, "venue_feed_down"), "before the event began");
        assert!(explained(75, "venue_feed_down"), "within the settle time");
        assert!(!explained(81, "venue_feed_down"), "after it");
    }
}
