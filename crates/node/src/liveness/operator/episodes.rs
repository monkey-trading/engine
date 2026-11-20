//! The alert episodes the operator's liveness rule keeps open.

use tracing::info;

/// The alert episodes the rule keeps open.
#[derive(Default)]
pub(super) struct Episodes {
    pub(super) read_failed: bool,
    pub(super) refused: bool,
    pub(super) start_refused: bool,
    pub(super) lagging: bool,
    /// The last read found no schedule (logged, not an alert).
    pub(super) unscheduled: bool,
}

/// Ends the episode `open` marks, logging `line`.
pub(super) fn resolve(open: &mut bool, alert: &'static str, line: &str) {
    if std::mem::take(open) {
        info!(alert_resolved = alert, "{line}");
    }
}
