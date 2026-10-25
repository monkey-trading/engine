//! A venue task's alerts: outages, silent symbols, a clock ahead of the host's, and refused
//! messages, each raised once per episode and resolved when it ends. Every alert names its
//! connection (`feed`, the endpoint URL), so the connections of one venue keep their own episodes.

use std::time::Duration;

use engine_core::types::{MarketId, Millis};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::clock::now_ms;
use crate::config::ApiKey;
use crate::feeds::venue::VenueFeed;

/// How much of a refused message its log line keeps.
const EXCERPT_CHARS: usize = 200;

impl VenueFeed {
    /// Records a fresh quote sequenced for `symbol`, ending any outage or silence it was in. A
    /// recovery restarts every symbol's silence count, since the outage alert covered the gap.
    pub(super) fn delivered(&mut self, symbol: &str) {
        let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
        let now = Instant::now();
        self.quoted = true;
        if self.down {
            self.down = false;
            self.last_quote.values_mut().for_each(|at| *at = now);
            info!(alert_resolved = "venue_feed_down", venue, feed, "venue feed recovered");
        }
        self.last_quote.insert(symbol.to_owned(), now);
        if self.silent.remove(symbol) {
            let market = self.instruments[symbol].market;
            info!(alert_resolved = "venue_symbol_silent", venue, feed, symbol, market, "venue symbol quoting again");
        }
    }

    /// Alerts once for each symbol that has gone `timing.timeout` without a fresh quote sequenced,
    /// across reconnects: one the venue accepted but never publishes, or whose quotes arrive stale
    /// or too far ahead of the host clock, or reach the engine late. An outage already alerted
    /// stands for every symbol. A symbol held back from the engine, or with its market closed, is not
    /// silent, and its count starts again from now.
    /// Returns whether a symbol went silent on this check, so the connection is replaced once to
    /// subscribe it afresh; a symbol already silent does not trigger another.
    pub(super) fn check_silent(&mut self) -> bool {
        if self.down {
            return false;
        }
        let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
        let now = Instant::now();
        let now_ms = now_ms();
        let mut newly_silent = false;
        for (symbol, instrument) in &self.instruments {
            let market = instrument.market;
            if self.is_held(symbol) || self.closed(market, now_ms) {
                self.last_quote.insert(symbol.clone(), now);
                continue;
            }
            let quiet = self.last_quote.get(symbol).is_some_and(|at| now.duration_since(*at) >= self.timing.timeout);
            if quiet && self.silent.insert(symbol.clone()) {
                warn!(
                    alert = "venue_symbol_silent",
                    venue, feed, symbol, market, "no fresh quote for {:?}", self.timing.timeout
                );
                newly_silent = true;
            }
        }
        newly_silent
    }

    /// Alerts, at most once per `REALERT_AFTER`, that a quote for `market` is dated `lead_ms` ahead
    /// of the host clock, past the `MAX_QUOTE_LEAD_MS` the engine accepts, so it was dropped: the
    /// venue's clock or the host's is wrong.
    ///
    /// TODO: the sequencer stamps with the untrusted host clock. One set back by more than
    /// `MAX_QUOTE_LEAD_MS` drops every quote, and the engine keeps pricing from the quotes it
    /// holds, judged against a sequenced time that a stalled host clock also stalls; the trusted
    /// time bound closes this.
    pub(super) fn clock_ahead(&mut self, market: MarketId, lead_ms: Millis) {
        if let Some(suppressed) = self.clock_alerts.ready(Instant::now()) {
            let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
            // The lead and the count change while the clock stays wrong, so they are not alert fields.
            warn!(
                alert = "venue_clock_ahead",
                venue,
                feed,
                market,
                "venue time {lead_ms}ms ahead of the host clock ({suppressed} more since the last alert)"
            );
        }
    }

    /// Logs a refused message, alerting at most once per `REALERT_AFTER`. The connection's API key
    /// never reaches the log, though a venue may echo it.
    pub(super) fn refuse(&mut self, reason: &str, text: &str) {
        let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
        let reason = self.redact(reason);
        let Some(suppressed) = self.refusals.ready(Instant::now()) else {
            debug!(venue, feed, reason, "venue message refused");
            return;
        };
        let excerpt: String = self.redact(text).chars().take(EXCERPT_CHARS).collect();
        // The reason and excerpt differ from message to message, so the connection alone is the key.
        warn!(
            alert = "venue_message_refused",
            venue, feed, "venue message refused ({suppressed} more since the last alert): {reason}: {excerpt}"
        );
    }

    /// Logs a lost connection. A connection that sequenced fresh quotes is a routine drop; one that
    /// sequenced none starts an outage, alerted once until a fresh quote ends it.
    pub(super) fn dropped(&mut self, error: &anyhow::Error, retry_in: Duration) {
        let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
        let error = self.redact(&format!("{error:#}"));
        if self.quoted {
            info!(venue, feed, ?retry_in, "venue feed dropped: {error}");
        } else if !self.down {
            self.down = true;
            warn!(alert = "venue_feed_down", venue, feed, "venue feed down, retrying in {retry_in:?}: {error}");
        } else {
            debug!(venue, feed, ?retry_in, "venue still down: {error}");
        }
    }

    /// `text` with the connection's API key replaced.
    fn redact(&self, text: &str) -> String {
        match self.api_key.as_ref().map(ApiKey::expose).filter(|key| !key.is_empty()) {
            Some(key) => text.replace(key, "<redacted>"),
            None => text.to_owned(),
        }
    }
}
