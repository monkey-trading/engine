//! A venue's protocol on one connection, whichever adapter speaks it.

use engine_core::types::Millis;

use crate::config::{ApiKey, Venue, VenueConnection};
use crate::feeds::venue::{Frame, Refusal};
use crate::feeds::{bitstamp, coinbase, databento, kraken, massive, tradermade};

/// A venue's protocol on one connection: its opening messages, and a reader that keeps any state
/// the venue's messages build on, such as an order book or a login handshake.
pub(super) enum Adapter {
    Bitstamp,
    Coinbase(coinbase::Books),
    Kraken(kraken::Books),
    Massive(massive::Session),
    Tradermade(tradermade::Session),
    Databento(databento::Session),
}

impl Adapter {
    /// A fresh adapter for a new connection on `conn`, authenticating with `api_key` where the
    /// venue needs one, and subscribed to `symbols`.
    pub(super) fn new(conn: &VenueConnection, api_key: Option<&ApiKey>, symbols: &[&str]) -> Self {
        let key = api_key.map(ApiKey::expose).unwrap_or_default();
        match conn.venue {
            Venue::Bitstamp => Adapter::Bitstamp,
            Venue::Coinbase => Adapter::Coinbase(coinbase::Books::new(symbols)),
            Venue::Kraken => Adapter::Kraken(kraken::Books::new(symbols)),
            Venue::Massive => Adapter::Massive(massive::Session::new(&conn.url, key, symbols)),
            Venue::Tradermade => Adapter::Tradermade(tradermade::Session::new(key, symbols)),
            Venue::Databento => {
                let dataset = conn.dataset.as_deref().unwrap_or_default();
                Adapter::Databento(databento::Session::new(key, dataset, symbols))
            }
        }
    }

    /// The text messages a new connection opens with: its subscriptions, or the login that leads
    /// to them.
    pub(super) fn opening(&self, symbols: &[&str]) -> Vec<String> {
        match self {
            Adapter::Bitstamp => bitstamp::subscriptions(symbols),
            Adapter::Coinbase(_) => coinbase::subscriptions(symbols),
            Adapter::Kraken(_) => kraken::subscriptions(symbols),
            Adapter::Massive(session) => session.opening(),
            Adapter::Tradermade(session) => session.opening(),
            Adapter::Databento(_) => Vec::new(),
        }
    }

    /// Whether the adapter keeps state built from earlier messages, so a refused book means that
    /// state can no longer be trusted.
    pub(super) fn keeps_state(&self) -> bool {
        matches!(self, Adapter::Coinbase(_) | Adapter::Kraken(_))
    }

    /// Reads one venue message at host time `now`.
    /// Error cases:
    /// - the message is not one the venue's protocol defines, a field fails to parse, or the
    ///   venue reported an error
    /// - the message cannot be applied to the state kept for this connection
    pub(super) fn read(&mut self, text: &str, now: Millis) -> Result<Frame, Refusal> {
        match self {
            Adapter::Bitstamp => bitstamp::parse(text).map_err(Refusal::Message),
            Adapter::Coinbase(books) => books.read(text, now),
            Adapter::Kraken(books) => books.read(text, now),
            Adapter::Massive(session) => session.read(text),
            Adapter::Tradermade(session) => session.read(text),
            Adapter::Databento(session) => session.read(text),
        }
    }
}
