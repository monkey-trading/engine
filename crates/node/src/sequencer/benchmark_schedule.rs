//! Durable schedule notifications, publisher synchronization at a changed rollover, and the end of
//! a hold no snapshot can end.

use engine_core::params::Rollover;
use engine_core::types::Millis;
use tracing::{info, warn};

use crate::sequencer::Sequencer;

impl Sequencer {
    /// Marks an accepted schedule change for publication after its log batch is flushed.
    pub(super) fn note_benchmark_schedule(&mut self, previous: Option<Rollover>) {
        if previous == self.engine.params().map(|params| params.rollover.clone()) {
            return;
        }
        self.benchmark_schedule_dirty = true;
        self.benchmark_schedule_pending |= self.benchmark_sync_enabled;
    }

    /// Holds a newly due rollover until a fresh complete signed publisher attempt is accepted.
    pub(super) fn admit_benchmark_schedule(&mut self, ts: Millis) {
        if self.benchmark_schedule_pending
            && !self.benchmark_sync
            && self.engine.next_rollover().is_some_and(|next| next <= ts)
        {
            self.benchmark_sync = true;
            warn!(alert = "benchmark_sync_pending", "waiting for signed publisher history after rollover changes");
        }
    }

    /// Ends a benchmark hold no snapshot can end by `ts` (see `hold_has_signer`): holding would also
    /// keep out the registry's mirror, and with it the key that could sign. Financing then uses the
    /// last applied rates until a snapshot applies.
    pub(super) fn lift_unsigned_hold(&mut self, ts: Millis) {
        if !self.benchmark_sync || self.hold_has_signer(ts) {
            return;
        }
        self.benchmark_sync = false;
        self.benchmark_schedule_pending = false;
        self.benchmark_sync_skipped = true;
        info!(alert_resolved = "benchmark_sync_pending", "no snapshot can come yet; sequencing resumed");
        warn!(
            alert = "benchmark_sync_skipped",
            "not waiting for signed publisher history: an installed operator's key has yet to take effect, so \
             the key the log records may sign nothing; financing uses the last applied rates until a snapshot \
             applies"
        );
    }

    /// Whether a snapshot can end a benchmark hold at `ts`. An installation supersedes the keys the
    /// log knows: one the finalised registry made by startup that the log's scan has not reached
    /// supersedes all but its own, due or not, and the log's installed handover waiting out its
    /// notice supersedes the key it records. Otherwise a key due by `ts` signs, since the engine hands
    /// over before it authorises the snapshot, and the recorded key does. Installing another key is
    /// the fallback for a failed operator whose key is gone, and the installed key signs nothing
    /// before its notice has run.
    fn hold_has_signer(&self, ts: Millis) -> bool {
        let due = self.engine.due_rotation(ts);
        if let Some(key) = self.registry_operator.and_then(|seated| seated.unscanned(&self.engine)) {
            return due.is_some_and(|due| due.address == key);
        }
        let installed = self.engine.operator_election().installed;
        let waiting = self.engine.operator_bounds().rotation.is_some_and(|rotation| {
            installed.is_some_and(|installed| {
                installed.admin_key == rotation.address && installed.effective_at == rotation.effective_at
            })
        });
        due.is_some() || !waiting
    }
}
