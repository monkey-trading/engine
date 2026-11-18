//! The state root worker's metrics (#925).

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

use crate::root_worker::MAX_READS;

/// What the sequencer measures of the state root's work on the root worker.
pub struct RootMetrics {
    pub backlog: Gauge,
    pub piece_seconds: Histogram,
    pub commit_seconds: Histogram,
    pub reads_refused: Counter,
}

impl Default for RootMetrics {
    fn default() -> Self {
        Self {
            backlog: Gauge::default(),
            // 50µs to about 1.6s.
            piece_seconds: Histogram::new(exponential_buckets(0.000_05, 2.0, 16)),
            commit_seconds: Histogram::new(exponential_buckets(0.000_05, 2.0, 16)),
            reads_refused: Counter::default(),
        }
    }
}

impl RootMetrics {
    /// Registers each metric under the `state_root_` names.
    pub(super) fn register(&self, r: &mut Registry) {
        r.register(
            "state_root_backlog",
            "What a checkpoint's state root would wait for now, in leaf writes on the root worker: those sent and \
             not yet written, those the engine has yet to copy out, and the commit's own work",
            self.backlog.clone(),
        );
        r.register(
            "state_root_piece_seconds",
            "Time the sequencer spends copying one piece of leaf writes out of its engine for the root worker",
            self.piece_seconds.clone(),
        );
        r.register(
            "state_root_commit_seconds",
            "Time the sequencer waits for the root worker's commit of a checkpoint's state root",
            self.commit_seconds.clone(),
        );
        r.register(
            "state_root_reads_refused",
            format!(
                "Proof and claim reads the sequencer answered unavailable, without forwarding them, because {MAX_READS} \
                 reads already waited on the root worker"
            ),
            self.reads_refused.clone(),
        );
    }
}
