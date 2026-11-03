//! How far behind a venue's stream can be, on the monotonic clock alone. The host carries the
//! bytes and sets the wall clock, so it can hold a stream back and set its clock back to match, and
//! a venue stamp then looks fresh. It cannot do that to a ping. A TLS stream is ordered and the host
//! cannot forge inside it, so the pong answering a ping sent at `t` arrives only after every frame
//! the venue sent before answering, all of them sent before `t` included. A frame read after that
//! pong was therefore sent after `t`, and is at most `now - t` old whatever the wall clock reads. A
//! frame read while that is over `timing.max_delay` counts as no quote, and a quote read in time
//! carries when it stops being so to the sequencer, since the host can hold that back too.
//!
//! This holds only while the monotonic clock's rate is honest (see `Clock::instant`), and only over
//! TLS: on Databento's plain TCP the host could answer a ping itself, so that connection carries
//! none, and the other source a market needs is what bounds it.

use std::collections::VecDeque;
use std::mem::take;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::time::{Instant, timeout};
use tracing::{info, warn};

use crate::feeds::venue::VenueFeed;
use crate::feeds::venue::transport::Link;

/// One connection's pings: those unanswered, and when the newest answered one was sent.
#[derive(Debug, Default)]
pub(super) struct DelayProbe {
    next_ping: u64,
    /// Pings sent and not yet answered, oldest first. A ping unanswered for `timing.timeout` ends
    /// the connection, which bounds how many there are.
    unanswered: VecDeque<(u64, Instant)>,
    newest_answered: Option<Instant>,
}

impl DelayProbe {
    /// Records a ping sent at `now` and returns the payload it carries.
    pub(super) fn sent(&mut self, now: Instant) -> Vec<u8> {
        let ping = self.next_ping;
        self.next_ping += 1;
        self.unanswered.push_back((ping, now));
        ping.to_be_bytes().to_vec()
    }

    /// Takes a pong carrying `payload`. An endpoint may answer only the latest ping it has read
    /// (RFC 6455 §5.5.3), so a pong answers the pings before its own too. One for no ping sent, as
    /// an unsolicited pong is, says nothing of this stream's position and is ignored.
    pub(super) fn answered(&mut self, payload: &[u8]) {
        let Some(ping) = <[u8; 8]>::try_from(payload).ok().map(u64::from_be_bytes) else { return };
        let answers = self.unanswered.iter().take_while(|(sent, _)| *sent <= ping).count();
        if self.unanswered.get(answers.wrapping_sub(1)).is_some_and(|(sent, _)| *sent == ping) {
            self.newest_answered = self.unanswered.drain(..answers).last().map(|(_, at)| at);
        }
    }

    /// How far behind the stream may be at `now`: the age of the newest answered ping. `None`
    /// before any is answered, when nothing bounds it.
    pub(super) fn behind(&self, now: Instant) -> Option<Duration> {
        self.newest_answered.map(|sent| now.saturating_duration_since(sent))
    }

    /// When the proof of a frame read now runs out: every frame read after a pong was sent after its
    /// ping, so the newest answered ping's send time bounds this frame's age from then on, and past
    /// `max_delay` of it the frame is too old to sequence.
    pub(super) fn not_after(&self, max_delay: Duration) -> Option<std::time::Instant> {
        self.newest_answered.map(|sent| (sent + max_delay).into_std())
    }

    /// How long the oldest unanswered ping has waited at `now`.
    fn waiting(&self, now: Instant) -> Option<Duration> {
        self.unanswered.front().map(|(_, sent)| now.saturating_duration_since(*sent))
    }
}

impl VenueFeed {
    /// Sends the connection's next ping, if it carries them.
    /// Error cases:
    /// - a ping has gone unanswered for `timing.timeout`
    /// - the ping cannot be sent within `timing.timeout`, or the connection fails
    pub(super) async fn ping(&mut self, link: &mut Link) -> anyhow::Result<()> {
        let Some(probe) = &mut self.delay else { return Ok(()) };
        let now = Instant::now();
        if let Some(waiting) = probe.waiting(now)
            && waiting >= self.timing.timeout
        {
            bail!("a ping unanswered for {waiting:?}");
        }
        let payload = probe.sent(now);
        timeout(self.timing.timeout, link.ping(payload)).await.context("ping timed out")?.context("ping")
    }

    /// Takes a pong read from the connection.
    pub(super) fn pong(&mut self, payload: &[u8]) {
        if let Some(probe) = &mut self.delay {
            probe.answered(payload);
        }
    }

    /// Whether a frame read now counts: on a connection without pings always, and otherwise while
    /// the stream is within `timing.max_delay`. Frames refused for `timing.max_delay` alert once,
    /// and the next that counts resolves it, so a late answer honest networks show raises nothing.
    pub(super) fn stream_current(&mut self) -> bool {
        let Some(probe) = &self.delay else { return true };
        let now = Instant::now();
        let max_delay = self.timing.max_delay;
        let behind = probe.behind(now);
        let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
        if behind.is_some_and(|behind| behind <= max_delay) {
            self.refused_since = None;
            if take(&mut self.delayed) {
                info!(alert_resolved = "venue_delayed", venue, feed, "venue stream back within its delay bound");
            }
            return true;
        }
        let refused = now.saturating_duration_since(*self.refused_since.get_or_insert(now));
        if refused >= max_delay && !self.delayed {
            self.delayed = true;
            // How far behind changes while the delay lasts, so it is in the message, not the key.
            let behind = behind.map_or("no ping answered".to_owned(), |behind| format!("{behind:?} behind"));
            warn!(
                alert = "venue_delayed",
                venue, feed, "venue stream past its {max_delay:?} delay bound ({behind}): its quotes do not count"
            );
        }
        false
    }
}

#[cfg(test)]
mod tests;
