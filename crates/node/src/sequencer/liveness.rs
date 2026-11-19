//! The sequencer's side of the liveness rule (#595): what holds it, which a v2 head is signed under,
//! and checkpoints that wait for another member to confirm holding their seq. A signed checkpoint is
//! public the moment it is written, and anyone holding the log may post it, so one past every member's
//! copy would leave a promoted member unable to start: it waits, with nothing stamped past its seq,
//! until a member confirms that seq. The wait wakes the rule, which asks the members at once and
//! again every few milliseconds, and a member syncs its copy as soon as its stream goes quiet, so the
//! pause lasts about one stream delivery, one sync and one round trip for a member current with the log.
//!
//! A member that trails the log, as one does while it moves to follow a new operator after a handover
//! or a promotion, or while the last read's confirmations have lapsed at a boundary, would hold every
//! command, prices included, until it catches up. So a paced checkpoint holds stamps for at most
//! `PACED_STALL`: past that it is given up on, stamping goes on, and the next paced attempt covers its
//! seq. Once paced checkpoints have been given up on for `DEFER_MAX`, the next waits for as long as a
//! member takes, under Warning `checkpoint_deferred`, so a set whose members trail the log still
//! checkpoints. A checkpoint that must not wait less, as a handover's final one, waits in full.

use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::log_heads::Hold;
use crate::sequencer::Sequencer;

/// How long a handover's stop waits for its final checkpoint's confirmation: a member confirms within a
/// stream delivery, a sync and a round trip.
const HANDOVER_WAIT: Duration = if cfg!(test) { Duration::from_millis(300) } else { Duration::from_secs(5) };
/// How long a paced checkpoint holds stamps for a member's confirmation before it is given up on. A
/// market keeps its venue quotes through a stall of up to half its `max_age_ms`, the margin the feeds
/// refresh a standing quote within (`feeds::venue::quotes::refresh_after`): this is half of that
/// margin at a one-second age.
pub(super) const PACED_STALL: Duration = Duration::from_millis(250);
/// How long paced checkpoints may be given up on before one waits in full.
pub(super) const DEFER_MAX: Duration = Duration::from_secs(30);

/// When the paced checkpoint that waits began to, and when paced checkpoints were first given up on
/// since one was written, with whether that is alerted.
#[derive(Debug, Default)]
pub(super) struct PacedWaits {
    since: Option<Instant>,
    deferred: Option<(Instant, bool)>,
}

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
            self.checkpoint_confirmed();
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
        if paced {
            self.paced_waits.since.get_or_insert_with(|| self.clock.instant());
        }
        true
    }

    /// Gives up on a paced checkpoint that has held stamps for `PACED_STALL` with no member confirming
    /// its seq: stamping goes on, and the next paced attempt covers it. Once paced checkpoints have been
    /// given up on for `DEFER_MAX`, the one waiting is held until a member confirms it, under Warning
    /// `checkpoint_deferred`.
    pub(super) fn give_up_stalled_checkpoint(&mut self) {
        let (Some(true), Some(since)) = (self.checkpoint_waits, self.paced_waits.since) else { return };
        let now = self.clock.instant();
        if now.saturating_duration_since(since) < PACED_STALL {
            return;
        }
        let (first, alerted) = self.paced_waits.deferred.get_or_insert((since, false));
        if now.saturating_duration_since(*first) >= DEFER_MAX {
            if !std::mem::replace(alerted, true) {
                // The seq moves on from episode to episode, so it is in the message, not the key.
                warn!(
                    alert = "checkpoint_deferred",
                    "paced checkpoints have waited {DEFER_MAX:?} for a member to confirm their seq: the one at seq {} \
                     holds every command until a member does",
                    self.engine.last_seq().unwrap_or_default()
                );
            }
            return;
        }
        debug!(seq = ?self.engine.last_seq(), "a paced checkpoint gave up waiting for a member's confirmation");
        (self.checkpoint_waits, self.paced_waits.since) = (None, None);
    }

    /// Ends the paced checkpoints' wait and deferral once a member confirmed a checkpoint's seq.
    fn checkpoint_confirmed(&mut self) {
        self.paced_waits.since = None;
        if let Some((_, true)) = self.paced_waits.deferred.take() {
            info!(alert_resolved = "checkpoint_deferred", "a member confirmed a checkpoint's seq");
        }
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
            self.give_up_stalled_checkpoint();
            return Ok(());
        }
        self.checkpoint_waits = None;
        self.checkpoint_confirmed();
        match paced {
            true => self.paced_checkpoint(),
            false => self.checkpoint(),
        }
    }
}

#[cfg(test)]
mod tests;
