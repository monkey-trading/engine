//! A quiet venue's books, re-dated by its pings. A venue sends a symbol's book only when it changes,
//! so a book that stands sends nothing, and its quote would age out of the engine while the venue
//! still shows it. A TLS stream is ordered, so the pong answering a ping arrives only after every
//! frame the venue sent before answering it (see `delay`). When a symbol's last book was read
//! before a ping went out and nothing has changed it by the ping's pong, the venue had sent no
//! change to that book by the time it answered, so its mid is sequenced again, stamped at the
//! venue's clock when the ping went out: the book's own stamp plus the time from its read to the
//! ping on the monotonic clock, and no later than the host clock. The stamp is fixed when the ping
//! goes out, so a host that delays the answer only ages the quote, and the ping's deadline bounds
//! when it may be sequenced, as for any frame read after that pong.
//!
//! A book is re-dated only by pings sent within `MAX_BOOK_SILENCE` of its read; after that the
//! venue's quote ages out as any other. A book the venue sends after a re-dated one is stamped no
//! earlier than it: the venue sent it after answering that ping, so it showed this book by then,
//! and the engine, which keeps the later-stamped of a venue's quotes, takes it in the re-dated one's
//! place.
//!
//! This rests on a venue writing its pong behind what it sent before reading the ping, which was
//! measured on Coinbase, Kraken and Bitstamp, so only their connections re-date books.

use std::collections::BTreeMap;
use std::time::Duration;

use engine_core::price::SourceQuote;
use engine_core::types::Millis;
use tokio::time::Instant;
use tracing::{debug, trace};

use crate::clock::now_ms;
use crate::config::Venue;
use crate::feeds::venue::adapter::Adapter;
use crate::feeds::venue::delay::DelayProbe;
use crate::feeds::venue::{Pending, VenueFeed};

/// How long after a symbol's book is read a ping's answer still re-dates it. It bounds how long a
/// venue whose book stream stalls while its connection still answers pings holds its leg of the
/// index: this plus the market's `max_age_ms` past the stalled book, and `max_delay` more if the
/// host delays the pongs, while the leg stays within the market's `max_divergence_bps` of the other
/// sources (`SECURITY.md`). Each second of it is a second a stalled book can price the market, so it
/// is the shortest whole second that covers the quiet books seen: in six hours of Coinbase, Kraken
/// and Bitstamp books, no gap between frames whose pings were answered lasted past 6 s, this plus a
/// 1 s `max_age_ms`, and 4 s would have left Coinbase's gaps of 5 to 6 s unpriced.
pub(super) const MAX_BOOK_SILENCE: Duration = Duration::from_secs(5);

/// The book each symbol last showed on the current connection, while a ping's answer may re-date
/// it, and the newest stamp an answer gave each symbol, kept across connections.
#[derive(Debug, Default)]
pub(super) struct HeldBooks {
    /// Whether the current connection re-dates books (see `redates`).
    redating: bool,
    books: BTreeMap<String, HeldBook>,
    /// A book read for a symbol after the stamp given it here is stamped no earlier.
    given: BTreeMap<String, Millis>,
}

/// A symbol's last book: its quote, when it was read, and the number of the first ping sent after.
#[derive(Debug)]
struct HeldBook {
    quote: SourceQuote,
    read_at: Instant,
    next_ping: u64,
}

/// A held book a ping's answer re-dates.
#[derive(Debug, PartialEq)]
pub(super) struct Redated {
    pub symbol: String,
    /// The stamp the book was read with.
    pub read_ts: Millis,
    /// Its quote, stamped afresh.
    pub quote: SourceQuote,
}

impl HeldBooks {
    /// Starts a new connection, which holds no book yet and re-dates books if `redating`.
    pub(super) fn connected(&mut self, redating: bool) {
        self.redating = redating;
        self.books.clear();
    }

    /// Holds `quote`, `symbol`'s book read now, on a connection that re-dates books and is pinged
    /// by `probe`.
    pub(super) fn hold(&mut self, symbol: &str, quote: &SourceQuote, probe: Option<&DelayProbe>) {
        if let Some(probe) = probe.filter(|_| self.redating) {
            self.hold_at(symbol, quote.clone(), Instant::now(), probe.next_ping());
        }
    }

    /// Holds `quote`, `symbol`'s book read at `read_at`, before the ping numbered `next_ping` went
    /// out.
    fn hold_at(&mut self, symbol: &str, quote: SourceQuote, read_at: Instant, next_ping: u64) {
        self.books.insert(symbol.to_owned(), HeldBook { quote, read_at, next_ping });
    }

    /// Forgets `symbol`'s book, as after one read for it that is not held.
    pub(super) fn forget(&mut self, symbol: &str) {
        self.books.remove(symbol);
    }

    /// Forgets every book, as after a refused message, which may have carried one.
    pub(super) fn clear(&mut self) {
        self.books.clear();
    }

    /// `quote`, a book just read for `symbol`, stamped no earlier than the newest stamp a ping's
    /// answer gave the symbol (see the module).
    pub(super) fn after_given(&self, symbol: &str, quote: SourceQuote) -> SourceQuote {
        match self.given.get(symbol) {
            Some(given) => SourceQuote { ts: quote.ts.max(*given), ..quote },
            None => quote,
        }
    }

    /// Forgets the books read more than `MAX_BOOK_SILENCE` before `sent`, returning their symbols.
    fn expire(&mut self, sent: Instant) -> Vec<String> {
        let expired: Vec<String> = self
            .books
            .iter()
            .filter(|(_, book)| sent.saturating_duration_since(book.read_at) > MAX_BOOK_SILENCE)
            .map(|(symbol, _)| symbol.clone())
            .collect();
        expired.iter().for_each(|symbol| self.forget(symbol));
        expired
    }

    /// The books the answer to ping `ping`, sent at `sent`, re-dates: each read before it went out,
    /// stamped at the venue's clock then — its stamp plus the time from its read to `sent` — and no
    /// later than host time `now`. A book that would gain nothing on its stamp, or on the stamp
    /// already given its symbol, is left out.
    fn redate(&self, ping: u64, sent: Instant, now: Millis) -> Vec<Redated> {
        self.books
            .iter()
            .filter(|(_, book)| book.next_ping <= ping)
            .filter_map(|(symbol, book)| {
                let quiet = Millis::try_from(sent.saturating_duration_since(book.read_at).as_millis()).ok()?;
                let ts = book.quote.ts.saturating_add(quiet).min(now);
                let gains = ts > book.quote.ts && self.given.get(symbol).is_none_or(|given| ts > *given);
                let quote = SourceQuote { ts, ..book.quote.clone() };
                gains.then(|| Redated { symbol: symbol.clone(), read_ts: book.quote.ts, quote })
            })
            .collect()
    }
}

impl VenueFeed {
    /// Re-dates into `buffered`, for `emit` to sequence, each held book the answer to ping `ping`,
    /// sent at `sent`, vouches for (see the module), noting its deadline from that ping. A book
    /// whose symbol is held back, whose market has not stayed open since its stamp, or whose kept
    /// book no longer tops out at its mid is forgotten instead. Nothing is re-dated once the answer
    /// itself is past `timing.max_delay`. Returns whether any book was.
    pub(super) fn redate_books(
        &mut self,
        (ping, sent): (u64, Instant),
        adapter: &Adapter,
        buffered: &mut BTreeMap<String, Pending>,
    ) -> bool {
        let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
        for symbol in self.held.expire(sent) {
            debug!(venue, feed, symbol, "venue book quiet for {MAX_BOOK_SILENCE:?}: no longer re-dated");
        }
        if Instant::now().saturating_duration_since(sent) > self.timing.max_delay {
            return false;
        }
        let now = now_ms();
        let mut redated = false;
        for Redated { symbol, read_ts, quote } in self.held.redate(ping, sent, now) {
            let market = self.instruments[&symbol].market;
            let open = self.schedules.get(&market).is_none_or(|schedule| schedule.open_throughout(read_ts, now));
            if self.is_held(&symbol) || !open || !adapter.tops_at(&symbol, quote.mid) {
                self.held.forget(&symbol);
                continue;
            }
            let ts = quote.ts;
            if buffered.entry(symbol.clone()).or_default().add(quote, now).is_ok() {
                let max_delay = self.timing.max_delay;
                self.delay.iter_mut().for_each(|probe| probe.note_read(&symbol, ts, max_delay));
                trace!(venue, feed, symbol, ts, "quiet book re-dated");
                self.held.given.insert(symbol, ts);
                redated = true;
            }
        }
        redated
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
