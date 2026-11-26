//! A test's check that a resolve line ends the episode an alert line opened, as the router decides it.

use tokio::time::Instant;

use crate::alerts::episodes::Episodes;
use crate::alerts::{AlertLine, Severity, Status};
use crate::test_support::Alert;

/// Whether the router, given the `raised` alert's fields as a test captured them and then the `resolution`'s,
/// pages the resolution as ending the alert's episode.
pub(crate) fn ends_episode(raised: &Alert, resolution: &Alert) -> bool {
    let mut episodes = Episodes::new(std::time::Duration::from_secs(60));
    let now = Instant::now();
    let Some(opened) = episodes.on_line(line(raised, Some(Severity::Warning)), now) else { return false };
    let ended = episodes.on_line(line(resolution, None), now);
    opened.status == Status::Firing
        && ended.is_some_and(|page| page.status == Status::Resolved && page.key == opened.key)
}

/// The line the alert layer would queue for the captured `fields`: every field but the message, the alert names and
/// the capture's own level identifies the episode.
fn line(fields: &Alert, severity: Option<Severity>) -> AlertLine {
    let name = fields.get("alert").or_else(|| fields.get("alert_resolved")).expect("an alert or a resolution").clone();
    let mut identity: Vec<(String, String)> = fields
        .iter()
        .filter(|(field, _)| !["alert", "alert_resolved", "message", "level"].contains(&field.as_str()))
        .map(|(field, value)| (field.clone(), value.clone()))
        .collect();
    identity.sort();
    let summary = fields.get("message").cloned().unwrap_or_default();
    AlertLine { name, severity, fields: identity, summary, context: String::new(), ts_ms: 0 }
}
