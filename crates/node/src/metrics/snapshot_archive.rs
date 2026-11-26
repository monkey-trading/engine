//! The daily snapshots' uploads to the archive's bucket (`daily_snapshots::upload`).

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

/// What the daily snapshots' uploader measures of the bucket it uploads to.
#[derive(Default)]
pub struct SnapshotArchiveMetrics {
    pub confirmed: Counter,
    pub pending: Gauge,
    pub removed: Counter,
    pub failures: Counter,
    pub mismatches: Counter,
}

impl SnapshotArchiveMetrics {
    /// Registers each metric under the `archive_snapshot` names.
    pub(super) fn register(&self, r: &mut Registry) {
        r.register(
            "archive_snapshots_confirmed",
            "Daily snapshots this process read back from the archive's bucket as their listing names them",
            self.confirmed.clone(),
        );
        r.register(
            "archive_snapshots_pending",
            "Listed daily snapshots on the host not yet confirmed in the archive's bucket",
            self.pending.clone(),
        );
        r.register(
            "archive_snapshots_removed",
            "Daily snapshots removed from the host, each older than the local window and confirmed in the bucket",
            self.removed.clone(),
        );
        r.register(
            "archive_snapshot_failures",
            "Uploads and read-backs of daily snapshots that failed and wait to be retried",
            self.failures.clone(),
        );
        r.register(
            "archive_snapshot_mismatches",
            "Daily snapshots whose object in the archive's bucket reads back as other bytes than their listing names",
            self.mismatches.clone(),
        );
    }
}
