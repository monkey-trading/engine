//! Holds the next checkpoint back while the last signed one is on its way to finality.
//!
//! The poster relays a checkpoint only once its signed parent is final, so the chain takes one per
//! finality delay however fast they are signed. One signed sooner only waits its turn, covering
//! nothing that arrived after it, while one signed when its parent is final covers everything up
//! to then. So at most one is on its way: the cadence follows the chain, and an approval waits for
//! the one on its way and its own, however many approvals arrive meanwhile.

use std::time::Instant;

use tracing::{debug, info, warn};

use crate::sequencer::checkpoints::Gate;

impl Gate {
    /// Whether the checkpoint at `seq` waits: a signed checkpoint is not yet final. A hold that
    /// outlasts `stalled_after` with no checkpoint becoming final raises `checkpoint_backlog`. Only
    /// the poster catching up resolves that: a checkpoint becoming final, or a paced attempt that
    /// finds none on its way. A checkpoint signed around the hold proves nothing about the poster.
    /// Error cases:
    /// - the checkpoint work registry is poisoned
    pub(super) fn holds_back(&mut self, seq: u64, now: Instant) -> anyhow::Result<bool> {
        let unfinal = match &self.registry {
            Some(registry) => registry
                .lock()
                .map_err(|_| anyhow::anyhow!("checkpoint work registry is poisoned"))?
                .range(self.finalized.map_or(0, |finalized| finalized + 1)..)
                .count(),
            None => 0,
        };
        if unfinal == 0 {
            self.clear_stall(format_args!("none is on its way at seq {seq}"));
            return Ok(false);
        }
        debug!(seq, unfinal, "checkpoint held back until the last signed one is final");
        let held_since = *self.held_since.get_or_insert(now);
        let stalled_for = now.duration_since(held_since);
        if stalled_for >= self.stalled_after && !self.stalled {
            self.stalled = true;
            self.earlier_run_open = false;
            if !self.queue_full {
                warn!(
                    alert = "checkpoint_backlog",
                    "checkpoint deferred at seq {seq}: none has become final for {stalled_for:?}"
                );
            }
        }
        Ok(true)
    }

    /// Records a checkpoint deferred at `seq` because the posting queue is full.
    pub(super) fn note_queue_full(&mut self, seq: u64) {
        if !std::mem::replace(&mut self.queue_full, true) {
            self.earlier_run_open = false;
            if !self.stalled {
                warn!(alert = "checkpoint_backlog", "checkpoint deferred at seq {seq}: the posting queue is full");
            }
        }
    }

    /// Records a checkpoint queued at `seq`, which ends a full queue's episode. A stall stays open:
    /// the checkpoint may have been signed around the hold.
    pub(super) fn note_queued(&mut self, seq: u64) {
        let was_open = self.backlog_open();
        self.queue_full = false;
        self.resolve_backlog(was_open, format_args!("a checkpoint was queued at seq {seq}"));
    }

    /// Ends the stall, and an earlier run's episode, now that the poster has caught up.
    fn clear_stall(&mut self, why: std::fmt::Arguments<'_>) {
        let was_open = self.backlog_open();
        (self.stalled, self.earlier_run_open) = (false, false);
        self.resolve_backlog(was_open, why);
    }

    /// Whether a `checkpoint_backlog` episode may be open, one an earlier run left included.
    fn backlog_open(&self) -> bool {
        self.stalled || self.queue_full || self.earlier_run_open
    }

    /// Logs the resolve once the last cause has cleared. With no episode open it pages nothing.
    fn resolve_backlog(&self, was_open: bool, why: std::fmt::Arguments<'_>) {
        if was_open && !self.backlog_open() {
            info!(alert_resolved = "checkpoint_backlog", "{why}");
        }
    }

    /// Takes `posted` as final. When it is newer, the poster has caught up: the stall bound of
    /// `holds_back` restarts and its episode ends.
    pub(super) fn note_final(&mut self, posted: u64) {
        if self.finalized.is_none_or(|finalized| finalized < posted) {
            self.finalized = Some(posted);
            self.held_since = None;
            self.clear_stall(format_args!("checkpoint {posted} became final"));
        }
    }
}
