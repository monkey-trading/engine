//! A quiet venue's standing books, re-dated by its pings. A venue sends a symbol's book only when
//! it changes, so a book that stands sends nothing, and its quote would age out of the engine while
//! the venue still shows it. A TLS stream is ordered, so the pong answering a ping arrives only after
//! every frame the venue sent before answering it (see `delay`). When a symbol's last book was read
//! before a ping went out and nothing has changed it by the ping's pong, the venue had sent no
//! change to that book by the time it answered, so its mid is sequenced again, stamped at the
//! venue's clock when the ping went out: the book's stamp as the engine stores it (no later than
//! the host clock when it was read) plus the time from its read to the ping on the monotonic clock,
//! and no later than the host clock read when the ping went out. The stamp is fixed when the ping
//! goes out, so a host that delays the answer only ages the quote, and the ping's deadline bounds
//! when it may be sequenced, as for any frame read after that pong.
//!
//! A book is re-dated only by pings sent within `MAX_BOOK_SILENCE` of the newest ping answered when
//! it was read, which went out before the venue sent the book, so a host that delays the book's
//! read gains nothing on it; after that the venue's quote ages out as any other. A book the venue
//! sends after a re-dated one is stamped no earlier than it: the venue sent it after answering that
//! ping, so it showed this book by then, and the engine, which keeps the later-stamped of a venue's
//! quotes, takes it in the re-dated one's place. When that stamp would lead the host clock by more
//! than the engine allows, as after the host clock steps back, the book goes at its own stamp in
//! place of any quote waiting for its symbol, which it changes.
//!
//! This rests on a venue writing its pong behind what it sent before reading the ping, which was
//! measured on Coinbase, Kraken and Bitstamp, so only their connections re-date books.

use std::collections::BTreeMap;
use std::time::Duration;

use engine_core::price::SourceQuote;
use engine_core::types::{MarketId, Millis};
use tokio::time::Instant;
use tracing::{debug, trace};

use crate::clock::now_ms;
use crate::config::Venue;
use crate::feeds::venue::adapter::Adapter;
use crate::feeds::venue::delay::{DelayProbe, Ping};
use crate::feeds::venue::{Pending, VenueFeed};

/// How long after the newest ping answered when a symbol's book was read a later ping's answer
/// still re-dates it. It bounds how long a venue whose book stream stalls while its connection still
/// answers pings holds its leg of the index: this plus `max_delay` plus the market's `max_age_ms`
/// past the venue's sending of the stalled book, about this plus `max_age_ms` when nothing delays
/// the pongs, while the leg stays within the market's `max_divergence_bps` of the other sources
/// (`SECURITY.md`). Each second of it is a second a stalled book can price the market. In six hours
/// of Coinbase, Kraken and Bitstamp books, re-dating cut Kraken's time without a fresh BTC-USD quote
/// from 5.4% to 2.1% with any limit from 4 s to 6 s, the rest being outages and stalls no pong
/// covers; 5 s keeps a second over the longest quiet book seen live, 4.2 s.
pub(super) const MAX_BOOK_SILENCE: Duration = Duration::from_secs(5);

/// The book each symbol last showed on the current connection, while a ping's answer may re-date
/// it, and the newest stamp an answer gave each symbol, kept across connections.
#[derive(Debug, Default)]
pub(super) struct StandingBooks {
    /// Whether the current connection re-dates books (see `redates`).
    redating: bool,
    books: BTreeMap<String, StandingBook>,
    /// A book read for a symbol after the stamp given it here is stamped no earlier.
    given: BTreeMap<String, Millis>,
}

/// A symbol's last book: its quote, when it was read, when the newest ping answered by then went
/// out, and the number of the first ping sent after.
#[derive(Debug)]
struct StandingBook {
    quote: SourceQuote,
    read_at: Instant,
    vouched_from: Instant,
    next_ping: u64,
}

/// A standing book a ping's answer re-dates.
#[derive(Debug, PartialEq)]
pub(super) struct Redated {
    pub symbol: String,
    /// The stamp the book was read with.
    pub read_ts: Millis,
    /// Its quote, stamped afresh.
    pub quote: SourceQuote,
}

impl StandingBooks {
    /// Starts a new connection, which holds no book yet and re-dates books if `redating`.
    pub(super) fn connected(&mut self, redating: bool) {
        self.redating = redating;
        self.books.clear();
    }

    /// Holds `quote`, `symbol`'s book read now, at host time `now`, on a connection that re-dates
    /// books and is pinged by `probe`, once a ping has been answered. It is held stamped as the
    /// engine stores it, no later than `now`, so a venue clock ahead of the host's still gains on it.
    pub(super) fn hold(&mut self, symbol: &str, quote: &SourceQuote, now: Millis, probe: Option<&DelayProbe>) {
        let Some(probe) = probe.filter(|_| self.redating) else { return };
        if let Some(vouched_from) = probe.newest_answered() {
            let quote = SourceQuote { ts: quote.ts.min(now), ..quote.clone() };
            self.hold_at(symbol, quote, Instant::now(), vouched_from, probe.next_ping());
        }
    }

    /// Holds `quote`, `symbol`'s book read at `read_at`, when the newest ping answered had gone out
    /// at `vouched_from`, before the ping numbered `next_ping` went out.
    fn hold_at(&mut self, symbol: &str, quote: SourceQuote, read_at: Instant, vouched_from: Instant, next_ping: u64) {
        self.books.insert(symbol.to_owned(), StandingBook { quote, read_at, vouched_from, next_ping });
    }

    /// Forgets `symbol`'s book, as after one read for it that is not held.
    pub(super) fn forget(&mut self, symbol: &str) {
        self.books.remove(symbol);
    }

    /// Forgets every book, as after a refused message, which may have carried one.
    pub(super) fn clear(&mut self) {
        self.books.clear();
    }

    /// The newest stamp a ping's answer gave `symbol`.
    fn given_for(&self, symbol: &str) -> Option<Millis> {
        self.given.get(symbol).copied()
    }

    /// Forgets the books whose window ran out before `sent`, returning their symbols.
    fn expire(&mut self, sent: Instant) -> Vec<String> {
        let expired: Vec<String> = self
            .books
            .iter()
            .filter(|(_, book)| sent.saturating_duration_since(book.vouched_from) > MAX_BOOK_SILENCE)
            .map(|(symbol, _)| symbol.clone())
            .collect();
        expired.iter().for_each(|symbol| self.forget(symbol));
        expired
    }

    /// The books the answer to `ping` re-dates: each read before it went out, stamped at the venue's
    /// clock then — its stamp plus the time from its read to the ping — and no later than the host
    /// clock then. A book that would gain nothing on its stamp, or on the stamp already given its
    /// symbol, is left out.
    fn redate(&self, ping: Ping) -> Vec<Redated> {
        self.books
            .iter()
            .filter(|(_, book)| book.next_ping <= ping.ping)
            .filter_map(|(symbol, book)| {
                let quiet = Millis::try_from(ping.sent.saturating_duration_since(book.read_at).as_millis()).ok()?;
                let ts = book.quote.ts.saturating_add(quiet).min(ping.sent_ms);
                let gains = ts > book.quote.ts && self.given_for(symbol).is_none_or(|given| ts > given);
                let quote = SourceQuote { ts, ..book.quote.clone() };
                gains.then(|| Redated { symbol: symbol.clone(), read_ts: book.quote.ts, quote })
            })
            .collect()
    }
}

impl VenueFeed {
    /// Re-dates into `buffered`, for `emit` to sequence, each standing book the answer to `ping`
    /// vouches for (see the module), noting its deadline from that ping. A book whose symbol is held
    /// back, whose market has not stayed open since its stamp, or whose kept book no longer tops out
    /// at its mid is forgotten instead. Nothing is re-dated once the answer itself is past
    /// `timing.max_delay`. Returns whether any book was.
    pub(super) fn redate_books(
        &mut self,
        ping: Ping,
        adapter: &Adapter,
        buffered: &mut BTreeMap<String, Pending>,
    ) -> bool {
        // Owned, so the alert for a refused quote can borrow the feed mutably.
        let url = self.conn.url.clone();
        let (venue, feed) = (self.conn.venue.name(), url.as_str());
        for symbol in self.standing.expire(ping.sent) {
            debug!(venue, feed, symbol, "venue book quiet for {MAX_BOOK_SILENCE:?}: no longer re-dated");
        }
        if Instant::now().saturating_duration_since(ping.sent) > self.timing.max_delay {
            return false;
        }
        let now = now_ms();
        let mut redated = false;
        for Redated { symbol, read_ts, quote } in self.standing.redate(ping) {
            let market = self.instruments[&symbol].market;
            let why = if self.is_held(&symbol) {
                Some("its symbol is held back")
            } else if !self.open_throughout(market, read_ts, now) {
                Some("its market has not stayed open since it")
            } else if !adapter.tops_at(&symbol, quote.mid) {
                Some("the kept book has no top at its mid")
            } else {
                None
            };
            if let Some(why) = why {
                debug!(venue, feed, symbol, "venue book no longer re-dated: {why}");
                self.standing.forget(&symbol);
                continue;
            }
            let ts = quote.ts;
            match buffered.entry(symbol.clone()).or_default().add(quote, now) {
                Ok(()) => {
                    let max_delay = self.timing.max_delay;
                    self.delay.iter_mut().for_each(|probe| probe.note_read(&symbol, ts, max_delay));
                    trace!(venue, feed, symbol, ts, "quiet book re-dated");
                    self.standing.given.insert(symbol, ts);
                    redated = true;
                }
                // The host clock stepped back since the ping went out, past the lead the engine
                // allows: refused as a book dated ahead is.
                Err(lead_ms) => {
                    self.standing.forget(&symbol);
                    self.clock_ahead(market, lead_ms);
                }
            }
        }
        redated
    }

    /// Buffers `quote`, a book just read for `symbol` at host time `now`, for `emit` to sequence,
    /// stamped no earlier than the newest stamp a ping's answer gave the symbol (`after_given`),
    /// and holds it to be re-dated. When that stamp leads `now` by more than the engine allows, as
    /// after the host clock steps back, the quote waiting for the symbol, which this book changes,
    /// is dropped and the book goes at its own stamp. Returns the stamp it is buffered at.
    /// Error cases:
    /// - the book's own stamp leads `now` by more than `MAX_QUOTE_LEAD_MS`, which the engine
    ///   refuses, so it is dropped and the symbol's standing book forgotten; the error is its lead
    pub(super) fn take_book(
        &mut self,
        symbol: &str,
        quote: SourceQuote,
        now: Millis,
        buffered: &mut BTreeMap<String, Pending>,
    ) -> Result<Millis, Millis> {
        let raised = self.after_given(symbol, quote.clone());
        let pending = buffered.entry(symbol.to_owned()).or_default();
        let taken = match pending.add(raised.clone(), now) {
            Err(_) if raised.ts != quote.ts => {
                pending.take();
                pending.add(quote.clone(), now).map(|()| quote)
            }
            added => added.map(|()| raised),
        };
        match taken {
            Ok(quote) => {
                self.standing.hold(symbol, &quote, now, self.delay.as_ref());
                Ok(quote.ts)
            }
            Err(lead_ms) => {
                self.standing.forget(symbol);
                Err(lead_ms)
            }
        }
    }

    /// `quote`, a book just read for `symbol`, stamped no earlier than the newest stamp a ping's
    /// answer gave the symbol (see the module), unless its market was not open throughout from its
    /// own stamp to that one: a book from a closure is not carried past it.
    fn after_given(&self, symbol: &str, quote: SourceQuote) -> SourceQuote {
        let market = self.instruments[symbol].market;
        match self.standing.given_for(symbol) {
            Some(given) if given > quote.ts && self.open_throughout(market, quote.ts, given) => {
                SourceQuote { ts: given, ..quote }
            }
            _ => quote,
        }
    }

    /// Whether the engine's last confirmed hours keep `market` open throughout `[from, to]`; a
    /// market whose hours are not known yet counts as open.
    fn open_throughout(&self, market: MarketId, from: Millis, to: Millis) -> bool {
        self.schedules.get(&market).is_none_or(|schedule| schedule.open_throughout(from, to))
    }
}

/// Whether `venue`'s connections re-date quiet books: those measured to write a pong behind what
/// they sent before reading its ping. Massive's and TraderMade's are not measured, and Databento's
/// gateway carries no pings.
pub(super) fn redates(venue: Venue) -> bool {
    matches!(venue, Venue::Bitstamp | Venue::Coinbase | Venue::Kraken)
}

#[cfg(test)]
mod tests;
