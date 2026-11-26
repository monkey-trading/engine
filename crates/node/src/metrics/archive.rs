//! The log archive's uploads to its bucket (`crate::archive`).

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::metrics::FloatGauge;

mod log_window;
pub use log_window::LogWindowMetrics;

/// What the archive's uploader measures of the bucket it uploads to.
#[derive(Default)]
pub struct ArchiveMetrics {
    pub confirmed_ranges: Gauge,
    pub pending_ranges: Gauge,
    pub oldest_pending_seconds: FloatGauge,
    pub objects_created: Counter,
    pub confirmed_bytes: Counter,
    pub stored_bytes: Counter,
    pub failures: Counter,
    pub mismatches: Counter,
    pub tail_seq: Gauge,
    /// The log's local window on the host.
    pub window: LogWindowMetrics,
}

impl ArchiveMetrics {
    /// Registers each metric under the `archive_` names, and the window's under the `log_local_` names.
    pub(super) fn register(&self, r: &mut Registry) {
        r.register(
            "archive_confirmed_ranges",
            "Ranges of the log, from the first, found in the archive's bucket when the uploader resumed or confirmed \
             there since as this node's copy holds them",
            self.confirmed_ranges.clone(),
        );
        r.register(
            "archive_pending_ranges",
            "Whole ranges of the log not yet confirmed in the archive's bucket, those waiting for the vault's \
             finalized checkpoint included",
            self.pending_ranges.clone(),
        );
        r.register(
            "archive_oldest_pending_seconds",
            "How long the oldest unconfirmed whole range, tail of a checkpoint, or resume that has not reached the \
             bucket, has waited, by the node's monotonic clock",
            self.oldest_pending_seconds.clone(),
        );
        r.register(
            "archive_objects_created",
            "Objects this node created in the archive's bucket",
            self.objects_created.clone(),
        );
        r.register(
            "archive_confirmed_bytes",
            "Bytes of the log confirmed in the archive's bucket, before compression",
            self.confirmed_bytes.clone(),
        );
        r.register(
            "archive_stored_bytes",
            "Bytes of the compressed ranges and tail pieces this node created in the archive's bucket",
            self.stored_bytes.clone(),
        );
        r.register(
            "archive_failures",
            "Uploads, read-backs, reads of the copy for checkpoints and reads of the vault's finality for the archive that \
             failed and wait to be retried",
            self.failures.clone(),
        );
        r.register(
            "archive_mismatches",
            "Objects in the archive's bucket that read back as other bytes than this node's copy of their range or tail \
             piece",
            self.mismatches.clone(),
        );
        r.register(
            "archive_tail_seq",
            "The newest checkpoint whose tail this node confirmed in the archive's bucket, through which the bucket \
             alone holds the log",
            self.tail_seq.clone(),
        );
        self.window.register(r);
    }
}
