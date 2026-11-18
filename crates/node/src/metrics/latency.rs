//! What a client waits for, on the monotonic clock: a venue quote from its receipt to its stamp, a
//! signed request from its admission to its reply, and a command from its stamp to its stream
//! publication; and how much of the time the sequencer's thread is busy. The budgets the launch bar
//! sets (1, 5, 10, 25, 50 and 100 ms) are bucket bounds, so the share of observations within each
//! is exact.

use std::sync::atomic::AtomicU64;
use std::time::Duration;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

/// The latency histograms' bucket bounds, in seconds: 50 µs to 5 s.
pub const LATENCY_BUCKETS: [f64; 16] =
    [0.000_05, 0.000_1, 0.000_25, 0.000_5, 0.001, 0.002_5, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];

/// The latencies a client sees, and the sequencer's busy time.
pub struct LatencyMetrics {
    pub price_to_stamp: Histogram,
    pub request: Histogram,
    pub stream_lag: Histogram,
    pub sequencer_busy: Counter<f64, AtomicU64>,
}

impl Default for LatencyMetrics {
    fn default() -> Self {
        Self {
            price_to_stamp: Histogram::new(LATENCY_BUCKETS),
            request: Histogram::new(LATENCY_BUCKETS),
            stream_lag: Histogram::new(LATENCY_BUCKETS),
            sequencer_busy: Counter::default(),
        }
    }
}

impl LatencyMetrics {
    /// Adds `spent` to the sequencer's busy time.
    pub fn busy(&self, spent: Duration) {
        self.sequencer_busy.inc_by(spent.as_secs_f64());
    }

    /// Registers each metric under its `engine_` name.
    pub(super) fn register(&self, r: &mut Registry) {
        r.register(
            "price_to_stamp_seconds",
            "Time from a venue quote's receipt at its venue task to its stamp; a repeat of the mid the engine holds, \
             held back to refresh it, counts from when the venue task sends it",
            self.price_to_stamp.clone(),
        );
        r.register(
            "api_request_seconds",
            "Time from the API admitting a signed request (a POST) past its client's rate to its reply",
            self.request.clone(),
        );
        r.register(
            "stream_lag_seconds",
            "Time from a command's stamp to its publication to stream subscribers, once its batch is durable",
            self.stream_lag.clone(),
        );
        r.register(
            "sequencer_busy_seconds",
            "Time the sequencer's thread spends sequencing batches and releasing them, not waiting for a request; its \
             rate is the share of the time the thread is busy",
            self.sequencer_busy.clone(),
        );
    }
}
