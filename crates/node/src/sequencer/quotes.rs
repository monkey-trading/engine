//! Stamping a command, and holding a venue's quote to its age bound at its stamp. The feed bounds a
//! quote's age when it reads it, but the host can hold the sequencer back after that, by stalling
//! its log or its CPU, so the bound is checked again on the very clock readings the stamp is made
//! from: a stall after them delays only that command, and any after it is stamped later.

use std::time::Instant;

use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::types::Millis;
use tracing::debug;

use crate::sequencer::Sequencer;

impl Sequencer {
    /// Sequences `cmd`, a venue's quote, unless the monotonic clock has passed `not_after`, or the
    /// run's stamps hold at its start, when a quote stored at that stamp would count for that much
    /// longer: dropped as no quote, it returns `None`, as for a stamp trusted time refuses.
    /// Error cases:
    /// - as `sequence`
    pub(super) fn sequence_quote(&mut self, cmd: Command, not_after: Instant) -> anyhow::Result<Option<Vec<Event>>> {
        self.sequence_stamped(cmd, Some(not_after))
    }

    /// The stamp for a command now, if trusted time admits it. The host clock is untrusted inside
    /// a TEE: it can't move sequenced time backwards or hold it back from monotonic time's pace, and
    /// nothing is logged unless the clock and the stamp are within the drift bound of trusted time.
    /// For a venue's quote, `quote` holds its `not_after`, and the stamp is refused, on the same
    /// readings it is made from, once the monotonic clock has passed that or while the run's stamps
    /// hold at its start.
    pub(super) fn admitted_ts(&mut self, quote: Option<Instant>) -> Option<Millis> {
        if self.frozen() {
            // A detected log rollback froze sequencing: stamps are refused, like an unadmitted
            // clock, until the node stops. The poster's latch is read directly, so what was
            // queued when it detected the rollback is refused without waiting for the `Freeze`
            // request to dequeue.
            return None;
        }
        // The liveness rule's gate: fenced by the enclave's own clock, before the start check, while
        // the log runs a heartbeat interval past the newest confirmation, or while a checkpoint waits
        // for one, nothing is stamped (`liveness`).
        if self.log_heads.as_ref().is_some_and(|publisher| !publisher.gate.sequences())
            || self.checkpoint_waits.is_some()
        {
            self.publish_hold();
            return None;
        }
        let (host_ms, now) = (self.clock.now_ms(), self.clock.instant());
        if let Some(not_after) = quote {
            if now > not_after {
                debug!("venue quote reached the sequencer past its delay bound: dropped");
                return None;
            }
            if self.time.holding_start(host_ms, now) {
                debug!(
                    "venue quote reached the sequencer while stamps hold past quotes carried across the restart: dropped"
                );
                return None;
            }
        }
        let ts = self.time.stamp(host_ms, self.last_ts, now);
        let admitted = self.time.admit(host_ms, ts, now);
        self.publish_hold();
        self.note_benchmark_time_admission(admitted, ts);
        if admitted {
            self.admit_benchmark_schedule(ts);
        }
        admitted.then_some(ts)
    }
}
