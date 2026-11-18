//! Resubscribing the symbols a venue stops quoting usably: silent (see `alerts`), or streamed too old
//! to count (see `stale`). They are resubscribed as soon as one is, and again for as long as any is,
//! whenever one shows it again since the last attempt or the connection's opening (a silent one by
//! `timing.timeout` more of silence, a stale one by its reads from then spanning the grace `stale`
//! sets) and a backoff has run since the last attempt, doubling from `timing.reconnect_min` to
//! `timing.reconnect_max` and starting again once none is.
//!
//! Where the venue takes a resubscription of some symbols alone (Coinbase), an attempt resubscribes
//! them alone, so the connection's other symbols quote on through it. Only when each has already
//! been resubscribed alone on this connection in its episode and one still reads stale is the
//! connection replaced, since a venue streaming a symbol from a source behind its own was seen to
//! reach a current one on a new connection; a symbol that is only silent, as a halted product is, is
//! resubscribed alone again rather than at the other symbols' cost. A venue that takes no
//! resubscription alone replaces the connection on every attempt.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::time::{Instant, timeout};
use tracing::info;

use crate::feeds::venue::adapter::Adapter;
use crate::feeds::venue::setup::next_backoff;
use crate::feeds::venue::transport::Link;
use crate::feeds::venue::{Timing, VenueFeed};

/// The last resubscription and the current connection's opening, the backoff the attempt after next
/// waits, and the symbols resubscribed alone on this connection in their current episode.
#[derive(Debug)]
pub(super) struct Resubscription {
    /// The last attempt and the backoff from it to the next, while symbols have needed one since.
    last: Option<(Instant, Duration)>,
    connected: Option<Instant>,
    wait: Duration,
    alone: BTreeSet<String>,
}

impl Resubscription {
    /// No attempt made, and the backoff at its start.
    pub(super) fn new(timing: &Timing) -> Self {
        Resubscription { last: None, connected: None, wait: timing.reconnect_min, alone: BTreeSet::new() }
    }

    /// Since when a silent symbol's silence counts towards the next attempt: the later of the last
    /// attempt and the current connection's opening.
    pub(super) fn since(&self) -> Option<Instant> {
        self.last.map(|(at, _)| at).max(self.connected)
    }

    /// Whether to resubscribe at `now` the symbols that need it (`needed`): once one has shown it
    /// again since the last attempt or connection (`shown`), and, after an attempt, once the backoff
    /// from it has run. Each attempt doubles the backoff for the one after. A symbol that no longer
    /// needs it ends its episode, and may be resubscribed alone again; once none needs it, the
    /// backoff starts again.
    pub(super) fn due(&mut self, needed: &[String], shown: bool, now: Instant, timing: &Timing) -> bool {
        self.alone.retain(|symbol| needed.contains(symbol));
        if needed.is_empty() {
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

    /// Records a connection opened at `now`, on which nothing has been resubscribed alone.
    pub(super) fn connected(&mut self, now: Instant) {
        self.connected = Some(now);
        self.alone.clear();
    }

    /// Whether to resubscribe `symbols` alone rather than replace the connection, noting them
    /// resubscribed alone: while one has not been yet on this connection in its episode, or none
    /// reads stale (`stale`).
    fn take_alone(&mut self, symbols: &[String], stale: bool) -> bool {
        let unspent = symbols.iter().any(|symbol| !self.alone.contains(symbol));
        self.alone.extend(symbols.iter().cloned());
        unspent || !stale
    }
}

impl VenueFeed {
    /// Starts a connection's counts: silent symbols' silence counts from now towards the next
    /// attempt, and stale runs are timed from their next read.
    pub(super) fn connected_afresh(&mut self) {
        self.resubscription.connected(Instant::now());
        self.stale.restart();
    }

    /// Resubscribes `symbols`: alone on `link` where the venue takes that, unless each has been
    /// already on this connection in its episode and one reads stale (see the module), so the other
    /// symbols quote on; otherwise by replacing the connection.
    /// Error cases:
    /// - the connection is to be replaced
    /// - the resubscription cannot be sent within `timing.timeout`
    pub(super) async fn resubscribe(
        &mut self,
        adapter: &Adapter,
        link: &mut Link,
        symbols: &[String],
    ) -> anyhow::Result<()> {
        let names: Vec<&str> = symbols.iter().map(String::as_str).collect();
        let stale = symbols.iter().any(|symbol| self.stale.alerted(symbol));
        let Some(messages) = adapter.resubscription(&names).filter(|_| self.resubscription.take_alone(symbols, stale))
        else {
            bail!("resubscribing {names:?}, silent or stale");
        };
        let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
        info!(venue, feed, symbols = ?names, "resubscribing silent or stale symbols alone");
        // Unsubscribed, the venue sends no change to their books, so a pong no longer shows them
        // unchanged (see `standing`).
        symbols.iter().for_each(|symbol| self.standing.forget(symbol));
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

    /// The symbols named, as `due` takes those that need resubscribing.
    fn named(symbols: &[&str]) -> Vec<String> {
        symbols.iter().map(|symbol| (*symbol).to_owned()).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn attempts_repeat_while_shown_again_each_after_a_doubling_backoff() {
        let timing = timing();
        let (eth, none) = (named(&["ETH-USD"]), named(&[]));
        let mut resubscription = Resubscription::new(&timing);
        let start = Instant::now();
        let after = |ms: u64| start + Duration::from_millis(ms);
        assert!(resubscription.due(&eth, true, start, &timing));
        // Backoffs of 0.5 s, 1 s, then 2 s, the cap.
        for (from, to) in [(0, 500), (500, 1_500), (1_500, 3_500), (3_500, 5_500)] {
            assert!(!resubscription.due(&eth, true, after(to - 1), &timing), "attempted before {to} ms");
            assert!(resubscription.due(&eth, true, after(to), &timing), "no attempt at {to} ms after {from} ms");
        }
        // A need not shown again since the last attempt waits however long.
        assert!(!resubscription.due(&eth, false, after(60_000), &timing));
        // Once none needs it, the backoff starts again and the next need is met at once.
        assert!(!resubscription.due(&none, false, after(60_001), &timing));
        assert!(resubscription.due(&eth, true, after(60_001), &timing));
        assert!(!resubscription.due(&eth, true, after(60_500), &timing));
        assert!(resubscription.due(&eth, true, after(60_501), &timing));
    }

    #[tokio::test(start_paused = true)]
    async fn a_symbol_is_resubscribed_alone_until_it_reads_stale_after_its_own_alone_attempt() {
        let timing = timing();
        let (sol, sol_eth, eth) = (named(&["SOL-USD"]), named(&["SOL-USD", "ETH-USD"]), named(&["ETH-USD"]));
        let mut resubscription = Resubscription::new(&timing);
        let start = Instant::now();
        // SOL is only silent, as a halted product is: alone every time.
        assert!(resubscription.due(&sol, true, start, &timing));
        assert!(resubscription.take_alone(&sol, false) && resubscription.take_alone(&sol, false));
        // ETH goes stale during SOL's episode: its own first attempt is alone, its next replaces.
        resubscription.due(&sol_eth, true, start + Duration::from_secs(1), &timing);
        assert!(resubscription.take_alone(&sol_eth, true));
        assert!(!resubscription.take_alone(&sol_eth, true));
        // A new connection, or ETH's episode ending, gives ETH an alone attempt again.
        resubscription.connected(start + Duration::from_secs(2));
        assert!(resubscription.take_alone(&sol_eth, true) && !resubscription.take_alone(&sol_eth, true));
        assert!(!resubscription.due(&sol, false, start + Duration::from_secs(3), &timing));
        assert!(resubscription.take_alone(&eth, true));
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
        assert!(resubscription.due(&named(&["ETH-USD"]), true, attempt, &timing));
        assert_eq!(resubscription.since(), Some(attempt));
        let reconnected = attempt + Duration::from_secs(1);
        resubscription.connected(reconnected);
        assert_eq!(resubscription.since(), Some(reconnected));
    }
}
