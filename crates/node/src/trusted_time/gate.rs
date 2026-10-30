//! The checks of the host clock against trusted time: the sequencer's of each stamp, and the API's
//! of the time it signs answers with.

use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_core::types::Millis;
use tokio::sync::watch;
use tracing::{debug, error, info};

use crate::clock::Clock;
use crate::trusted_time::{Sample, ceil_millis, floor_millis};

/// How long sequencing may go on after the latest trusted time sample (#15 §4).
pub const NO_SAMPLE_GRACE: Duration = Duration::from_secs(60);

/// Why sequencing is halted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Halt {
    /// Waiting, within the grace, for the run's first sample: a normal start.
    Starting,
    Drift,
    NoSample,
}

/// Why a stamp was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// The stamp, or the host clock reading it came from, is not within the drift bound of every
    /// time the sample allows.
    Drift { host_ms: Millis, earliest_ms: Millis, latest_ms: Millis },
    /// No sample for `waited`: none since the start, or the latest is older than the grace.
    NoSample { waited: Duration, sampled: bool },
}

/// Admits a stamp only while it, and the host clock reading it came from, lie within `max_drift`
/// of every time the latest sample allows. Once the latest sample is older than `NO_SAMPLE_GRACE`,
/// nothing passes until a fresh one arrives. The grace of #15 §4 runs from the latest sample of
/// this run, and a run starts with none: nothing passes before its first sample, so a restart,
/// which the host controls, never opens a window on the host clock alone. Waiting for the first
/// sample alerts only once the grace has passed.
pub struct TimeGate {
    max_drift_ms: Millis,
    started: Instant,
    sample: Option<Sample>,
    halted: Option<Halt>,
    /// Stamps refused since sequencing halted.
    refused: u64,
}

/// The host clock, read only while the gate would admit a stamp of it: within `max_drift` of every
/// time the latest trusted time sample allows, with that sample no older than `NO_SAMPLE_GRACE`. The
/// API signs answers with it.
pub struct TrustedClock {
    clock: Arc<dyn Clock>,
    samples: watch::Receiver<Option<Sample>>,
    max_drift_ms: Millis,
}

impl Halt {
    /// The alert this halt raises, and the line that resolves it when the halt ends; a normal start
    /// raises none.
    fn alert(self) -> Option<(&'static str, &'static str)> {
        match self {
            Halt::Starting => None,
            Halt::Drift => Some(("clock_drift", "the host clock is back within the drift bound of trusted time")),
            Halt::NoSample => Some(("trusted_time_unavailable", "a trusted time sample is back within the grace")),
        }
    }
}

impl Refusal {
    /// The halt this refusal puts sequencing in.
    fn halt(self) -> Halt {
        match self {
            Refusal::Drift { .. } => Halt::Drift,
            Refusal::NoSample { waited, sampled: false } if waited <= NO_SAMPLE_GRACE => Halt::Starting,
            Refusal::NoSample { .. } => Halt::NoSample,
        }
    }
}

impl TimeGate {
    /// A gate with no sample yet, started at `now`.
    pub fn new(max_drift: Duration, now: Instant) -> Self {
        Self { max_drift_ms: floor_millis(max_drift), started: now, sample: None, halted: None, refused: 0 }
    }

    /// Replaces the latest sample.
    pub fn update(&mut self, sample: Sample) {
        self.sample = Some(sample);
    }

    /// Checks admission without changing alerts, including just before a new sample replaces it.
    pub(crate) fn can_admit(&self, host_ms: Millis, ts: Millis, now: Instant) -> bool {
        self.check(host_ms, ts, now).is_none()
    }

    /// Whether `ts`, stamped from the host clock reading `host_ms`, may be logged at monotonic time
    /// `now`. Raises a Critical alert when sequencing halts, or halts for another reason, and logs
    /// when it resumes.
    pub fn admit(&mut self, host_ms: Millis, ts: Millis, now: Instant) -> bool {
        let refusal = self.check(host_ms, ts, now);
        let halt = refusal.map(Refusal::halt);
        // A halt that ends, or gives way to another, resolves the alert it raised.
        if self.halted != halt
            && let Some((alert, cleared)) = self.halted.and_then(Halt::alert)
        {
            info!(alert_resolved = alert, "{cleared}");
        }
        match refusal {
            None if self.halted == Some(Halt::Starting) => {
                info!(refused = self.refused, "first trusted time sample: sequencing started");
                self.refused = 0;
            }
            None if self.halted.is_some() => {
                info!(refused = self.refused, "stamps are within the drift bound of trusted time: sequencing resumed");
                self.refused = 0;
            }
            None => {}
            Some(r) => {
                if self.halted != halt && halt != Some(Halt::Starting) {
                    self.alert(ts, r);
                }
                debug!(ts, refusal = ?r, "stamp refused: sequencing halted");
                self.refused += 1;
            }
        }
        self.halted = halt;
        refusal.is_none()
    }

    /// Why `ts`, stamped from `host_ms`, may not be logged at `now`, if it may not.
    fn check(&self, host_ms: Millis, ts: Millis, now: Instant) -> Option<Refusal> {
        let Some(sample) = self.sample else {
            return Some(Refusal::NoSample { waited: now.saturating_duration_since(self.started), sampled: false });
        };
        check_sample(&sample, self.max_drift_ms, host_ms, ts, now)
    }

    /// Raises the Critical alert for a refusal that halts sequencing.
    fn alert(&self, ts: Millis, refusal: Refusal) {
        match refusal {
            // The readings change from stamp to stamp, so they go in the message, not the episode key.
            Refusal::Drift { host_ms, earliest_ms, latest_ms } => error!(
                alert = "clock_drift",
                "host clock or stamp is not within the drift bound of trusted time: sequencing halted \
                 (stamp {ts}, host {host_ms}, trusted {earliest_ms} to {latest_ms}, bound {}ms)",
                self.max_drift_ms
            ),
            Refusal::NoSample { waited, sampled } => error!(
                alert = "trusted_time_unavailable",
                "no trusted time sample within the grace: sequencing halted (waited {}ms, sampled before: \
                 {sampled})",
                floor_millis(waited)
            ),
        }
    }
}

impl TrustedClock {
    /// Reads `clock` against the latest sample `samples` holds, within `max_drift`.
    pub fn new(clock: Arc<dyn Clock>, samples: watch::Receiver<Option<Sample>>, max_drift: Duration) -> Self {
        Self { clock, samples, max_drift_ms: floor_millis(max_drift) }
    }

    /// The host clock's reading, if the gate would admit a stamp of it now; `None` before the first
    /// sample, once the latest is older than the grace, or while the reading is beyond the bound.
    pub fn now_ms(&self) -> Option<Millis> {
        let sample = (*self.samples.borrow())?;
        let host_ms = self.clock.now_ms();
        check_sample(&sample, self.max_drift_ms, host_ms, host_ms, self.clock.instant()).is_none().then_some(host_ms)
    }

    /// The latest sample's upper bound moved on by the monotonic time since, however old the sample;
    /// `None` before the first. For date checks that tolerate the monotonic clock's drift while no
    /// fresh sample comes, as when a node has stopped sequencing to hand its keys over.
    pub fn since_sample_ms(&self) -> Option<Millis> {
        let sample = (*self.samples.borrow())?;
        let elapsed = self.clock.instant().saturating_duration_since(sample.at);
        Some(sample.latest_ms.saturating_add(ceil_millis(elapsed)))
    }
}

/// Why `ts`, stamped from `host_ms`, may not be logged at `now` under `sample`, if it may not: the
/// sample is older than the grace, or either time is not within `max_drift_ms` of every time the
/// sample allows.
fn check_sample(sample: &Sample, max_drift_ms: Millis, host_ms: Millis, ts: Millis, now: Instant) -> Option<Refusal> {
    let age = now.saturating_duration_since(sample.at);
    if age > NO_SAMPLE_GRACE {
        return Some(Refusal::NoSample { waited: age, sampled: true });
    }
    // True time has moved on since the sample by the monotonic time elapsed.
    let earliest_ms = sample.earliest_ms.saturating_add(floor_millis(age));
    let latest_ms = sample.latest_ms.saturating_add(ceil_millis(age));
    let within =
        |t: Millis| latest_ms.saturating_sub(max_drift_ms) <= t && t <= earliest_ms.saturating_add(max_drift_ms);
    (!(within(host_ms) && within(ts))).then_some(Refusal::Drift { host_ms, earliest_ms, latest_ms })
}
