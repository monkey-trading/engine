//! Resubscribing the symbols a venue stops quoting usably: silent (see `alerts`), or streamed too old
//! to count (see `stale`). They are resubscribed as soon as one is, and again for as long as any is,
//! whenever one shows it again since the last attempt or the connection's opening (a silent one by
//! `timing.timeout` more of silence, a stale one by its reads from then spanning the grace `stale`
//! sets) and a backoff has run since the last attempt, doubling from `timing.reconnect_min` to
//! `timing.reconnect_max` and starting again once none is. Where the venue takes a resubscription
//! of some symbols alone (Coinbase), the first attempt on a connection resubscribes them alone, so
//! the other symbols quote on through it; a further attempt on the same connection replaces the
//! connection, since a venue streaming a symbol from a source behind its own was seen to reach a
//! current one on a new connection.

use std::time::Duration;

use anyhow::{Context, bail};
use tokio::time::{Instant, timeout};
use tracing::info;

use crate::feeds::venue::adapter::Adapter;
use crate::feeds::venue::setup::next_backoff;
use crate::feeds::venue::transport::Link;
use crate::feeds::venue::{Timing, VenueFeed};

/// The last resubscription and the current connection's opening, and the backoff the attempt after
/// next waits.
#[derive(Debug)]
pub(super) struct Resubscription {
    /// The last attempt and the backoff from it to the next, while symbols have needed one since.
    last: Option<(Instant, Duration)>,
    connected: Option<Instant>,
    wait: Duration,
}

impl Resubscription {
    /// No attempt made, and the backoff at its start.
    pub(super) fn new(timing: &Timing) -> Self {
        Resubscription { last: None, connected: None, wait: timing.reconnect_min }
    }

    /// Since when a silent symbol's silence counts towards the next attempt: the later of the last
    /// attempt and the current connection's opening.
    pub(super) fn since(&self) -> Option<Instant> {
        self.last.map(|(at, _)| at).max(self.connected)
    }

    /// Whether to resubscribe at `now`, while symbols need it (`needed`): once one has shown it
    /// again since the last attempt (`shown`), or none was made, and the backoff from the last has
    /// run. Each attempt doubles the backoff for the one after; once none needs it, it starts again.
    pub(super) fn due(&mut self, needed: bool, shown: bool, now: Instant, timing: &Timing) -> bool {
        if !needed {
            (self.last, self.wait) = (None, timing.reconnect_min);
            return false;
        }
        if !shown || self.last.is_some_and(|(at, wait)| now.duration_since(at) < wait) {
            return false;
        }
        self.last = Some((now, self.wait));
        self.wait = next_backoff(self.wait, timing.reconnect_max);
        true
    }

    /// Records a connection opened at `now`.
    pub(super) fn connected(&mut self, now: Instant) {
        self.connected = Some(now);
    }
}

impl VenueFeed {
    /// Starts a connection's counts: silent symbols' silence counts from now towards the next
    /// attempt, and stale runs are timed from their next read.
    pub(super) fn connected_afresh(&mut self) {
        self.resubscription.connected(Instant::now());
        self.stale.restart();
    }

    /// Resubscribes `symbols`: alone on `link` where the venue takes that and `alone`, this
    /// connection's record, shows it has not yet, so the other symbols quote on; otherwise by
    /// replacing the connection.
    /// Error cases:
    /// - the connection is to be replaced
    /// - the resubscription cannot be sent within `timing.timeout`
    pub(super) async fn resubscribe(
        &mut self,
        adapter: &Adapter,
        link: &mut Link,
        symbols: &[String],
        alone: &mut bool,
    ) -> anyhow::Result<()> {
        let names: Vec<&str> = symbols.iter().map(String::as_str).collect();
        let Some(messages) = adapter.resubscription(&names).filter(|_| !*alone) else {
            bail!("resubscribing {names:?}, silent or stale");
        };
        *alone = true;
        let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
        info!(venue, feed, symbols = ?names, "resubscribing silent or stale symbols alone");
        timeout(self.timing.timeout, link.send_all(messages))
            .await
            .context("resubscribe timed out")?
            .context("resubscribe")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feeds::tests::fake::fast;

    fn timing() -> Timing {
        Timing {
            reconnect_min: Duration::from_millis(500),
            reconnect_max: Duration::from_secs(2),
            ..fast(Duration::from_secs(10))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn attempts_repeat_while_shown_again_each_after_a_doubling_backoff() {
        let timing = timing();
        let mut resubscription = Resubscription::new(&timing);
        let start = Instant::now();
        let after = |ms: u64| start + Duration::from_millis(ms);
        assert!(resubscription.due(true, true, start, &timing));
        // Backoffs of 0.5 s, 1 s, then 2 s, the cap.
        for (from, to) in [(0, 500), (500, 1_500), (1_500, 3_500), (3_500, 5_500)] {
            assert!(!resubscription.due(true, true, after(to - 1), &timing), "attempted before {to} ms");
            assert!(resubscription.due(true, true, after(to), &timing), "no attempt at {to} ms after {from} ms");
        }
        // A need not shown again since the last attempt waits however long.
        assert!(!resubscription.due(true, false, after(60_000), &timing));
        // Once none needs it, the backoff starts again and the next need is met at once.
        assert!(!resubscription.due(false, false, after(60_001), &timing));
        assert!(resubscription.due(true, true, after(60_001), &timing));
        assert!(!resubscription.due(true, true, after(60_500), &timing));
        assert!(resubscription.due(true, true, after(60_501), &timing));
    }

    #[tokio::test(start_paused = true)]
    async fn silence_counts_from_the_later_of_the_last_attempt_and_the_connection() {
        let timing = timing();
        let mut resubscription = Resubscription::new(&timing);
        let start = Instant::now();
        assert_eq!(resubscription.since(), None);
        resubscription.connected(start);
        assert_eq!(resubscription.since(), Some(start));
        let attempt = start + Duration::from_secs(10);
        assert!(resubscription.due(true, true, attempt, &timing));
        assert_eq!(resubscription.since(), Some(attempt));
        let reconnected = attempt + Duration::from_secs(1);
        resubscription.connected(reconnected);
        assert_eq!(resubscription.since(), Some(reconnected));
    }
}
