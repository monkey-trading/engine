//! The sequencer's side of the liveness rule (#595): what holds it, which a v2 head is signed under,
//! and checkpoints that wait for another member to confirm holding their seq. A signed checkpoint is
//! public the moment it is written, and anyone holding the log may post it, so one past every member's
//! copy would leave a promoted member unable to start: it waits, with nothing stamped past its seq,
//! until a member confirms that seq. The wait wakes the rule, which asks the members at once and
//! again every few milliseconds, and a member syncs its copy as soon as its stream goes quiet, so the
//! pause lasts about one stream delivery, one sync and one round trip.

use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tracing::debug;

use crate::log_heads::Hold;
use crate::sequencer::Sequencer;

/// How long a handover's stop waits for its final checkpoint's confirmation: a member confirms within a
/// stream delivery, a sync and a round trip.
const HANDOVER_WAIT: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(5) };

impl Sequencer {
    /// Whether an admitted request's next command seals nothing: sequencing froze, or the liveness
    /// rule's fence closed since its admission (a log append the host held, say).
    pub(super) fn stopped(&self) -> bool {
        self.frozen() || self.log_heads.as_ref().is_some_and(|publisher| !publisher.gate.signs())
    }

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

    /// Whether the checkpoint at `seq` must wait: the liveness rule applies and no member has confirmed
    /// that far, the start check has not passed (a member may hold more than this log, or another
    /// history), or the fence stops the enclave signing, checkpoints included. The attempt is
    /// remembered, required if any attempt was, the rule is woken to ask for a confirmation, and
    /// nothing is stamped until `release_waiting_checkpoint` writes it.
    pub(super) fn checkpoint_waits_for(&mut self, seq: u64, paced: bool) -> bool {
        let Some(publisher) = &self.log_heads else { return false };
        let (confirmed, fenced) = (publisher.gate.confirmed(), !publisher.gate.signs());
        let unchecked = !publisher.gate.start_checked();
        if confirmed >= seq && !fenced && !unchecked {
            return false;
        }
        debug!(
            seq,
            confirmed,
            fenced,
            unchecked,
            "the checkpoint waits for a member's confirmation, the start check or the fence"
        );
        if !fenced {
            publisher.gate.want(seq);
        }
        self.checkpoint_waits = Some(self.checkpoint_waits.map_or(paced, |was| was && paced));
        true
    }

    /// Parks a handover's stop while its final checkpoint waits for a member's confirmation, with
    /// nothing stamped meanwhile, for at most `HANDOVER_WAIT`.
    pub(super) fn park_handover(&mut self, reply: oneshot::Sender<anyhow::Result<()>>) {
        debug!("the handover's final checkpoint waits for a member to confirm its seq");
        self.handover_waits = Some((reply, Instant::now() + HANDOVER_WAIT));
    }

    /// The parked handover's stop, once its final checkpoint is written; refused, and sequencing goes
    /// on, once `HANDOVER_WAIT` runs out first.
    pub(super) fn admitted_handover(&mut self) -> Option<oneshot::Sender<anyhow::Result<()>>> {
        let (reply, until) = self.handover_waits.take()?;
        if self.checkpoint_waits.is_none() && self.last_checkpoint == self.engine.last_seq() {
            return Some(reply);
        }
        if Instant::now() < until {
            self.handover_waits = Some((reply, until));
            return None;
        }
        let refused = crate::sequencer::HandoverRefused::new(format!(
            "the final checkpoint waited {HANDOVER_WAIT:?} for another member of the turn's set to confirm its seq: the \
             handover is refused until one does"
        ));
        let _ = reply.send(Err(anyhow::anyhow!(refused)));
        None
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
        if publisher.gate.confirmed() < seq || !publisher.gate.signs() || !publisher.gate.start_checked() {
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
