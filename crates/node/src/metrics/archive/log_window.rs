//! The log's local window on the host (`crate::archive::window`), which runs beside the archive's uploader.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::metrics::FloatGauge;

/// What the local window measures of the log's segments on the host.
#[derive(Default)]
pub struct LogWindowMetrics {
    pub local_bytes: Gauge,
    pub local_first_seq: Gauge,
    pub held_seconds: FloatGauge,
    pub removed: Counter,
    pub restored: Counter,
    pub failures: Counter,
    pub passes: Counter,
}

impl LogWindowMetrics {
    /// Registers each metric under the `log_local_` names.
    pub(super) fn register(&self, r: &mut Registry) {
        r.register(
            "log_local_bytes",
            "Bytes of the log's segments on the host, as their listings and the open segment's flushed prefix give them",
            self.local_bytes.clone(),
        );
        r.register(
            "log_local_first_seq",
            "The first seq of the oldest segment of the log on the host; the archive's bucket holds the log before it",
            self.local_first_seq.clone(),
        );
        r.register(
            "log_local_held_seconds",
            "Sequenced seconds by which the oldest segment on the host is past archive.local_window, 0 within it",
            self.held_seconds.clone(),
        );
        r.register(
            "log_local_segments_removed",
            "Segments the local window removed from the host once the bucket gave back their bytes",
            self.removed.clone(),
        );
        r.register(
            "log_local_segments_restored",
            "Segments the host lacked that the local window put back from the archive",
            self.restored.clone(),
        );
        r.register(
            "log_local_window_failures",
            "Passes of the local window over the log's segments on the host that failed and run again",
            self.failures.clone(),
        );
        r.register(
            "log_local_window_passes",
            "Passes of the local window over the log's segments on the host, each once it ends, failed or not",
            self.passes.clone(),
        );
    }
}
