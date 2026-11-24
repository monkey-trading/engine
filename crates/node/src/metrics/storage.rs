//! The disk watcher's metrics: each store's bytes and each data volume's space and growth.

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::metrics::FloatGauge;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct StoreLabel {
    pub store: &'static str,
}

/// A data volume, named by the first store on it.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct VolumeLabel {
    pub volume: String,
}

/// What the node reads of its disks, as the host's filesystem reports them.
#[derive(Default)]
pub struct StorageMetrics {
    pub store_bytes: Family<StoreLabel, Gauge>,
    pub volume_total_bytes: Family<VolumeLabel, Gauge>,
    pub volume_free_bytes: Family<VolumeLabel, Gauge>,
    pub volume_growth_bytes_per_second: Family<VolumeLabel, FloatGauge>,
    pub volume_fill_seconds: Family<VolumeLabel, FloatGauge>,
}

impl StorageMetrics {
    /// Registers each metric.
    pub(super) fn register(&self, r: &mut Registry) {
        r.register(
            "store_bytes",
            "Bytes the host's disk allocates to each store's files, by store (log, history_index, marks_cache, \
         recovery_caches, daily_snapshots, alert_outbox), as the host's filesystem reports them: an operator \
         figure, never a protocol input",
            self.store_bytes.clone(),
        );
        r.register(
            "volume_total_bytes",
            "Size of each data volume the stores sit on, named by its first store, as the host reports it",
            self.volume_total_bytes.clone(),
        );
        r.register(
            "volume_free_bytes",
            "Bytes of each data volume available to the node, as the host reports them: an operator figure, never a \
         protocol input",
            self.volume_free_bytes.clone(),
        );
        r.register(
            "volume_growth_bytes_per_second",
            "Growth of each volume's used bytes over alerts.storage.rate_window, negative while it shrinks; absent \
         until the window is full",
            self.volume_growth_bytes_per_second.clone(),
        );
        r.register(
            "volume_fill_seconds",
            "Seconds until each volume is full at its growth over alerts.storage.rate_window; absent while it is \
         not growing or the window is not yet sampled",
            self.volume_fill_seconds.clone(),
        );
    }
}
