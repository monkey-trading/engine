//! A venue's protocol on one connection, whichever adapter speaks it.

use std::time::Duration;

use engine_core::Fixed;
use engine_core::types::Millis;

use crate::config::{ApiKey, Venue, VenueConnection};
use crate::feeds::venue::validation::mid_of;
use crate::feeds::venue::{Frame, Refusal};
use crate::feeds::{bitstamp, cme, coinbase, kraken, massive, tradermade};

/// A venue's protocol on one connection: its opening messages, and a reader that keeps any state
/// the venue's messages build on, such as an order book or a login handshake.
pub(super) enum Adapter {
    Bitstamp,
    Cme(cme::Session),
    Coinbase(coinbase::Books),
    Kraken(kraken::Books),
    Massive(massive::Session),
    Tradermade(tradermade::Session),
}

impl Adapter {
    /// A fresh adapter for a new connection on `conn`, authenticating with `api_key` where the
    /// venue needs one, and subscribed to `symbols`.
    pub(super) fn new(conn: &VenueConnection, api_key: Option<&ApiKey>, symbols: &[&str]) -> Self {
        let key = api_key.map(ApiKey::expose).unwrap_or_default();
        match conn.venue {
            Venue::Bitstamp => Adapter::Bitstamp,
            Venue::Cme => Adapter::Cme(cme::Session::new(symbols)),
            Venue::Coinbase => Adapter::Coinbase(coinbase::Books::new(symbols)),
            Venue::Kraken => Adapter::Kraken(kraken::Books::new(symbols)),
            Venue::Massive => Adapter::Massive(massive::Session::new(&conn.url, key, symbols)),
            Venue::Tradermade => Adapter::Tradermade(tradermade::Session::new(key, symbols)),
        }
    }

    /// The text messages a new connection opens with: its subscriptions, or the login that leads
    /// to them.
    pub(super) fn opening(&self, symbols: &[&str]) -> Vec<String> {
        match self {
            Adapter::Bitstamp => bitstamp::subscriptions(symbols),
            // CME logs in on the handshake and answers with its login status, before the subscription.
            Adapter::Cme(_) => Vec::new(),
            Adapter::Coinbase(_) => coinbase::subscriptions(symbols),
            Adapter::Kraken(_) => kraken::subscriptions(symbols),
            Adapter::Massive(session) => session.opening(),
            Adapter::Tradermade(session) => session.opening(),
        }
    }

    /// How long a read may wait for the venue's next message: `timeout`, or for CME, which sends only
    /// a heartbeat every 10 seconds while a closure leaves it nothing else to send, at least two of
    /// its heartbeat intervals, so a late heartbeat does not end a working connection.
    pub(super) fn read_timeout(&self, timeout: Duration) -> Duration {
        match self {
            Adapter::Cme(_) => timeout.max(cme::HEARTBEAT * 2),
            _ => timeout,
        }
    }

    /// Whether the adapter keeps state built from earlier messages, so a refused book means that
    /// state can no longer be trusted.
    pub(super) fn keeps_state(&self) -> bool {
        matches!(self, Adapter::Coinbase(_) | Adapter::Kraken(_))
    }

    /// Whether the book kept for `symbol` still tops out at `mid`. A message that leaves a kept book
    /// with an empty side yields no book, so only the kept book shows that its top has gone. Any
    /// other adapter reads each top whole, so the last book it read is its top.
    pub(super) fn tops_at(&self, symbol: &str, mid: Fixed) -> bool {
        let top = match self {
            Adapter::Coinbase(books) => books.top(symbol),
            Adapter::Kraken(books) => books.top(symbol),
            _ => return true,
        };
        top.is_some_and(|(bid, ask)| mid_of(bid, ask) == mid)
    }

    /// Reads one venue message at host time `now`.
    /// Error cases:
    /// - the message is not one the venue's protocol defines, a field fails to parse, or the
    ///   venue reported an error
    /// - the message cannot be applied to the state kept for this connection
    pub(super) fn read(&mut self, text: &str, now: Millis) -> Result<Frame, Refusal> {
        match self {
            Adapter::Bitstamp => bitstamp::parse(text).map_err(Refusal::Message),
            Adapter::Cme(session) => session.read(text),
            Adapter::Coinbase(books) => books.read(text, now),
            Adapter::Kraken(books) => books.read(text, now),
            Adapter::Massive(session) => session.read(text),
            Adapter::Tradermade(session) => session.read(text),
        }
    }
}
