//! The sequencer's side of the liveness rule (#595): what holds it, which a v2 head is signed under,
//! and checkpoints that wait for another member to confirm holding their seq. A signed checkpoint is
//! public the moment it is written, and anyone holding the log may post it, so one past every member's
//! copy would leave a promoted member unable to start: it waits, with nothing stamped past its seq,
//! until a member confirms that seq.

use tracing::debug;

use crate::log_heads::Hold;
use crate::sequencer::Sequencer;

impl Sequencer {
    /// Tells the head poster what holds the sequencer: a trusted-time halt, a benchmark-sync hold, or
    /// nothing. No other stop is a hold.
    pub(super) fn publish_hold(&self) {
        let Some(publisher) = &self.log_heads else { return };
        let hold = if self.time.halted() {
            Hold::TimeHalt
        } else if self.benchmark_sync {
            Hold::BenchmarkSync
        } else {
            Hold::Live
        };
        publisher.hold.send_if_modified(|was| std::mem::replace(was, hold) != hold);
    }

    /// Whether the checkpoint at `seq` must wait for a member to confirm holding it: the liveness rule
    /// applies and no member has confirmed that far. The attempt is remembered, and nothing is stamped
    /// until `release_waiting_checkpoint` writes it.
    pub(super) fn checkpoint_waits_for(&mut self, seq: u64, paced: bool) -> bool {
        let Some(publisher) = &self.log_heads else { return false };
        let confirmed = publisher.gate.confirmed();
        if confirmed >= seq {
            return false;
        }
        debug!(seq, confirmed, "the checkpoint waits for a member to confirm holding its seq");
        self.checkpoint_waits = Some(self.checkpoint_waits.unwrap_or(false) || paced);
        true
    }

    /// Writes the checkpoint that waited, once a member confirmed holding its seq.
    /// Error cases:
    /// - as `paced_checkpoint`
    pub(super) fn release_waiting_checkpoint(&mut self) -> anyhow::Result<()> {
        let (Some(paced), Some(publisher), Some(seq)) =
            (self.checkpoint_waits, &self.log_heads, self.engine.last_seq())
        else {
            return Ok(());
        };
        if publisher.gate.confirmed() < seq {
            return Ok(());
        }
        self.checkpoint_waits = None;
        match paced {
            true => self.paced_checkpoint(),
            false => self.checkpoint(),
        }
    }
}

#[cfg(test)]
mod tests;
