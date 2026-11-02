//! Holds the next checkpoint back while the last signed one is on its way to finality.
//!
//! The poster relays a checkpoint only once its signed parent is final, so the chain takes one per
//! finality delay however fast they are signed. One signed sooner only waits its turn, covering
//! nothing that arrived after it, while one signed when its parent is final covers everything up
//! to then. So at most one is on its way: the cadence follows the chain, and an approval waits for
//! the one on its way and its own, however many approvals arrive meanwhile.

use std::time::Instant;

use tracing::{debug, warn};

use crate::sequencer::checkpoints::Gate;

impl Gate {
    /// Whether the checkpoint at `seq` waits: a signed checkpoint is not yet final. A hold that
    /// outlasts `stalled_after` with no checkpoint becoming final raises `checkpoint_backlog`, which
    /// the next signed checkpoint resolves.
    /// Error cases:
    /// - the checkpoint work registry is poisoned
    pub(super) fn holds_back(&mut self, seq: u64, now: Instant) -> anyhow::Result<bool> {
        let Some(registry) = &self.registry else { return Ok(false) };
        let unfinal = registry
            .lock()
            .map_err(|_| anyhow::anyhow!("checkpoint work registry is poisoned"))?
            .range(self.finalized.map_or(0, |finalized| finalized + 1)..)
            .count();
        if unfinal == 0 {
            return Ok(false);
        }
        debug!(seq, unfinal, "checkpoint held back until the last signed one is final");
        let held_since = *self.held_since.get_or_insert(now);
        let stalled_for = now.duration_since(held_since);
        if stalled_for >= self.stalled_after && !std::mem::replace(&mut self.backlogged, true) {
            warn!(
                alert = "checkpoint_backlog",
                "checkpoint deferred at seq {seq}: none has become final for {stalled_for:?}"
            );
        }
        Ok(true)
    }

    /// Takes `posted` as final, which restarts the stall bound of `holds_back` when it is newer.
    pub(super) fn note_final(&mut self, posted: u64) {
        if self.finalized.is_none_or(|finalized| finalized < posted) {
            self.finalized = Some(posted);
            self.held_since = None;
        }
    }
}
