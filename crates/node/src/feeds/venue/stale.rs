//! Books a venue sends already too old to count. A venue can stream one symbol from a source minutes
//! behind its own while the other symbols on the connection are current and its pings are answered
//! at once, as Coinbase did on 16 November 2026, so the delay bound (see `delay`) sees a current
//! stream. Each such book carries the venue's own time, so the engine never counts its quote, but
//! the symbol's market goes without this venue until it is resubscribed (see `resubscribe`).
//!
//! A book read after the newest answered ping was sent after that ping went out (see `delay`), so its
//! age less the time since that ping went out is how old, at least, the venue's book already was
//! when the venue sent it. A read counts as stale by that alone: neither a large snapshot's transfer,
//! which makes it a second or two old when read, nor a stream the host holds back reads as a venue
//! streaming from behind. A run of such reads that spans the shorter of `STALE_GRACE` and
//! `timing.timeout` raises `venue_stale` and starts that resubscription; the next read that is not
//! stale ends it. The run is timed by its reads, so an old book followed by none, as a quiet book's
//! snapshot can be, is left to the silence count (see `alerts`), which takes its re-dated quotes
//! into account. Each resubscription and new connection times every run afresh from its next read,
//! so the next attempt waits on what follows it.

use std::collections::BTreeMap;
use std::time::Duration;

use engine_core::types::Millis;
use tokio::time::Instant;
use tracing::info;

use crate::feeds::venue::VenueFeed;

/// How long a symbol's books may read stale before the venue counts as streaming it from behind: a
/// few reads' worth, so one late burst does not set off a resubscription, and well inside the
/// silence count's `timing.timeout`.
pub(in crate::feeds) const STALE_GRACE: Duration = Duration::from_secs(2);

/// Each symbol's current run of reads too old to count.
#[derive(Debug, Default)]
pub(super) struct StaleRuns {
    runs: BTreeMap<String, Run>,
}

/// A run of reads too old to count: when its first read since it was last timed afresh and its
/// newest were read, how old the newest was, and whether it was alerted.
#[derive(Debug)]
struct Run {
    first: Option<Instant>,
    last: Instant,
    age_ms: Millis,
    alerted: bool,
}

impl StaleRuns {
    /// Notes a book for `symbol` read at `now`, `age_ms` old by its venue time, where `bound_ms` is
    /// the oldest its quote counts at: past it, the symbol's run starts or goes on; within it, the
    /// run ends. Returns whether it ended an alerted run.
    pub(super) fn read(&mut self, symbol: &str, age_ms: Millis, bound_ms: Millis, now: Instant) -> bool {
        if age_ms <= bound_ms {
            return self.end(symbol);
        }
        let run = self.runs.entry(symbol.to_owned()).or_insert(Run { first: None, last: now, age_ms, alerted: false });
        run.first.get_or_insert(now);
        (run.last, run.age_ms) = (now, age_ms);
        false
    }

    /// The age of `symbol`'s newest read when its run's reads span `grace` and it is not yet
    /// alerted, marking it alerted.
    pub(super) fn newly_stale(&mut self, symbol: &str, grace: Duration) -> Option<Millis> {
        let run = self.runs.get_mut(symbol).filter(|run| !run.alerted && run.spans(grace))?;
        run.alerted = true;
        Some(run.age_ms)
    }

    /// Whether `symbol`'s run has read too old to count across `grace` since it was last timed
    /// afresh.
    pub(super) fn spans(&self, symbol: &str, grace: Duration) -> bool {
        self.runs.get(symbol).is_some_and(|run| run.spans(grace))
    }

    /// Times every run afresh from its next read, keeping whether it was alerted.
    pub(super) fn restart(&mut self) {
        self.runs.values_mut().for_each(|run| run.first = None);
    }

    /// Whether `symbol` is in a run that was alerted.
    pub(super) fn alerted(&self, symbol: &str) -> bool {
        self.runs.get(symbol).is_some_and(|run| run.alerted)
    }

    /// Ends `symbol`'s run, returning whether it was alerted.
    pub(super) fn end(&mut self, symbol: &str) -> bool {
        self.runs.remove(symbol).is_some_and(|run| run.alerted)
    }
}

impl Run {
    /// Whether its reads since it was last timed afresh span `grace`.
    fn spans(&self, grace: Duration) -> bool {
        self.first.is_some_and(|first| self.last.duration_since(first) >= grace)
    }
}

impl VenueFeed {
    /// Notes how old, at least, `symbol`'s book stamped `ts` and read at host time `now` was when the
    /// venue sent it (see the module), against the oldest a quote counts at (see `quotes::counts`),
    /// resolving `venue_stale` when it ends an alerted run. Before any ping is answered, the whole
    /// age counts.
    pub(super) fn note_age(&mut self, symbol: &str, ts: Millis, now: Millis) {
        let Some(market) = self.instruments.get(symbol).map(|instrument| instrument.market) else { return };
        let stale_after = Millis::try_from(self.timing.stale_after.as_millis()).unwrap_or(Millis::MAX);
        let bound_ms = self.max_age.get(&market).map_or(stale_after, |max_age| stale_after.min(*max_age));
        let read_at = Instant::now();
        let behind =
            self.delay.behind(read_at).map_or(0, |behind| Millis::try_from(behind.as_millis()).unwrap_or(Millis::MAX));
        let sent_age = now.saturating_sub(ts).saturating_sub(behind);
        if self.stale.read(symbol, sent_age, bound_ms, read_at) {
            let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
            info!(alert_resolved = "venue_stale", venue, feed, symbol, market, "venue quotes current again");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: Duration = STALE_GRACE;

    #[tokio::test(start_paused = true)]
    async fn a_run_of_reads_too_old_to_count_alerts_once_they_span_its_grace_and_a_current_read_ends_it() {
        let mut stale = StaleRuns::default();
        let start = Instant::now();
        assert!(!stale.read("ETH-USD", 600_000, 1_000, start));
        assert!(!stale.read("ETH-USD", 600_100, 1_000, start + GRACE - Duration::from_millis(1)));
        assert_eq!(stale.newly_stale("ETH-USD", GRACE), None);
        assert!(!stale.read("ETH-USD", 600_200, 1_000, start + GRACE));
        // The age reported is the newest read's.
        assert_eq!(stale.newly_stale("ETH-USD", GRACE), Some(600_200));
        assert!(stale.alerted("ETH-USD"));
        assert!(!stale.read("ETH-USD", 600_300, 1_000, start + GRACE * 2));
        assert_eq!(stale.newly_stale("ETH-USD", GRACE), None);
        // A read at the bound counts, so it ends the alerted run.
        assert!(stale.read("ETH-USD", 1_000, 1_000, start + GRACE * 2));
        assert!(!stale.alerted("ETH-USD"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_its_reads_do_not_span_ends_or_waits_without_an_alert() {
        let mut stale = StaleRuns::default();
        let start = Instant::now();
        // A snapshot and the updates behind it read late, then current, then late again.
        assert!(!stale.read("BTC-USD", 2_400, 1_000, start));
        assert!(!stale.read("BTC-USD", 900, 1_000, start + Duration::from_secs(1)));
        assert!(!stale.read("BTC-USD", 2_400, 1_000, start + Duration::from_secs(2)));
        // The second run counts from its own first read.
        assert!(!stale.read("BTC-USD", 2_400, 1_000, start + Duration::from_millis(3_999)));
        assert_eq!(stale.newly_stale("BTC-USD", GRACE), None);
        // A quiet book's old snapshot, with no read after it, never alerts however long it stands.
        assert!(!stale.read("SOL-USD", 60_000, 1_000, start));
        tokio::time::advance(GRACE * 10).await;
        assert_eq!(stale.newly_stale("SOL-USD", GRACE), None);
        assert!(!stale.end("BTC-USD") && !stale.end("SOL-USD"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_restarted_run_is_timed_from_its_next_read_and_stays_alerted() {
        let mut stale = StaleRuns::default();
        let start = Instant::now();
        stale.read("ETH-USD", 600_000, 1_000, start);
        stale.read("ETH-USD", 600_000, 1_000, start + GRACE);
        assert_eq!(stale.newly_stale("ETH-USD", GRACE), Some(600_000));
        stale.restart();
        assert!(!stale.spans("ETH-USD", GRACE) && stale.alerted("ETH-USD"));
        // A new subscription's snapshot reads two seconds late a while after the restart.
        let snapshot = start + GRACE * 3;
        stale.read("ETH-USD", 2_000, 1_000, snapshot);
        assert!(!stale.spans("ETH-USD", GRACE));
        stale.read("ETH-USD", 600_000, 1_000, snapshot + GRACE - Duration::from_millis(1));
        assert!(!stale.spans("ETH-USD", GRACE));
        stale.read("ETH-USD", 600_000, 1_000, snapshot + GRACE);
        assert!(stale.spans("ETH-USD", GRACE));
        // Alerted once for the episode.
        assert_eq!(stale.newly_stale("ETH-USD", GRACE), None);
    }
}
