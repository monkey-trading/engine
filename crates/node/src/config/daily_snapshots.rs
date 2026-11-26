//! The daily snapshots' section (`[daily_snapshots]`).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::ensure;
use serde::Deserialize;

/// A day.
const DAY: Duration = Duration::from_secs(86_400);

/// Where the node writes the engine's state at the first checkpoint it writes on each UTC day by sequenced time,
/// which `GET /v1/archive/snapshots/{file}` serves for a replay's ranges.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DailySnapshotsSection {
    /// A directory of its own, apart from the log, the history index, the recovery caches and the alert
    /// outbox, since a start removes the files in it that are named as snapshots but unlisted.
    pub dir: PathBuf,
    /// With `[archive]`, how long the host keeps a snapshot once the bucket holds it: one stamped more than this
    /// before the newest listed leaves the host, but for the newest such one, which a replay of the window's
    /// commands starts from. Without `[archive]`, every day's snapshot is kept.
    #[serde(with = "humantime_serde", default = "default_local_window")]
    pub local_window: Duration,
}

impl DailySnapshotsSection {
    /// Checks that with `[archive]` (`archived`) the window reaches back as far as `retention`, the history index's:
    /// the history re-derives an indexed command's rows from a snapshot before it that the host holds.
    /// Error cases:
    /// - the bucket keeps the snapshots and the window is shorter than the history's retention
    pub(crate) fn validate(&self, archived: bool, retention: Duration) -> anyhow::Result<()> {
        ensure!(
            !archived || self.local_window >= retention,
            "daily_snapshots.local_window must be at least history.retention, {retention:?}, so the host keeps a \
             snapshot before every command the history index holds"
        );
        Ok(())
    }
}

#[cfg(test)]
impl DailySnapshotsSection {
    /// The section for `dir`, keeping the default window.
    pub(crate) fn at(dir: PathBuf) -> Self {
        Self { dir, local_window: default_local_window() }
    }
}

/// Thirty days, the history index's retention in the release configurations, so the host keeps a snapshot before
/// every command it indexes.
fn default_local_window() -> Duration {
    DAY * 30
}
