//! Kraken spot WebSocket v2. The `book` channel sends each symbol's top ten levels a side as a
//! snapshot, then every change to them, stamped with the venue time. The adapter keeps those
//! levels and, after every message, checks them against the CRC32 checksum Kraken sends with it.
//! Kraken's book carries no sequence number, so the checksum is also what catches a dropped
//! message: the next one no longer matches. A book that fails the check is dropped and the
//! connection resyncs for a fresh snapshot, and no mid is taken from it. Every verified change
//! publishes the top of book, so the venue keeps quoting while its top holds.
//!
//! Prices and sizes arrive as JSON numbers written at the pair's precision. They are read from
//! the raw number text, never through a float, and that text is what the checksum covers.

use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;
use crc32fast::Hasher;
use engine_core::Fixed;
use engine_core::types::Millis;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;

use crate::feeds::venue::{Book, Frame, Refusal, check_time};

/// The levels a side that Kraken's book subscription keeps and its checksum covers.
const DEPTH: usize = 10;

/// The verified top levels of every symbol on one connection.
pub(super) struct Books {
    /// The symbols the connection subscribed to; a book for any other is refused before it is
    /// kept, so the venue cannot grow books the feed never reads.
    symbols: BTreeSet<String>,
    depths: BTreeMap<String, Depth>,
}

/// One symbol's levels, keyed by price.
#[derive(Default)]
struct Depth {
    bids: BTreeMap<Fixed, Level>,
    asks: BTreeMap<Fixed, Level>,
}

/// A level's price and size as Kraken wrote them, the text its checksum is computed over.
struct Level {
    price: String,
    qty: String,
}

/// Every Kraken message: a channel message, or a reply to a method call.
#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow)]
    channel: Option<&'a str>,
    #[serde(borrow, rename = "type")]
    kind: Option<&'a str>,
    #[serde(borrow)]
    method: Option<&'a str>,
    success: Option<bool>,
    error: Option<String>,
    #[serde(borrow)]
    data: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct BookData<'a> {
    symbol: String,
    #[serde(borrow)]
    bids: Vec<RawLevel<'a>>,
    #[serde(borrow)]
    asks: Vec<RawLevel<'a>>,
    checksum: u32,
    timestamp: &'a str,
}

#[derive(Deserialize)]
struct RawLevel<'a> {
    #[serde(borrow)]
    price: &'a RawValue,
    #[serde(borrow)]
    qty: &'a RawValue,
}

impl Books {
    /// Empty books for a connection subscribed to `symbols`.
    pub(super) fn new(symbols: &[&str]) -> Self {
        let symbols = symbols.iter().map(|symbol| (*symbol).to_owned()).collect();
        Books { symbols, depths: BTreeMap::new() }
    }

    /// Reads one Kraken message at host time `now`, applying and verifying any book change in it.
    /// Error cases:
    /// - an unknown channel or method, or a failed subscription (the connection carries on)
    /// - a `book` message that does not parse or apply, or whose checksum does not match the
    ///   kept book (the connection resyncs)
    pub(super) fn read(&mut self, text: &str, now: Millis) -> Result<Frame, Refusal> {
        let message: Message =
            serde_json::from_str(text).map_err(|e| Refusal::Message(format!("unparseable message: {e}")))?;
        match (message.channel, message.method) {
            (Some("book"), None) => {
                let data = message.data.ok_or_else(|| Refusal::Connection("book without data".into()))?;
                self.apply(message.kind, data.get(), now).map(Frame::Books).map_err(Refusal::Connection)
            }
            (Some("heartbeat" | "status"), None) => Ok(Frame::Control),
            (None, Some("subscribe")) if message.success == Some(true) => Ok(Frame::Control),
            (None, Some(method)) => {
                Err(Refusal::Message(format!("venue error on {method}: {}", message.error.unwrap_or_default())))
            }
            _ => Err(Refusal::Message("unexpected message".into())),
        }
    }

    /// The best bid and ask kept for `symbol`, while its book has both sides.
    pub(super) fn top(&self, symbol: &str) -> Option<(Fixed, Fixed)> {
        let depth = self.depths.get(symbol)?;
        depth.bids.last_key_value().zip(depth.asks.first_key_value()).map(|((bid, _), (ask, _))| (*bid, *ask))
    }

    /// Applies a `book` message and returns each verified top of book. A book with an empty side
    /// has no top.
    /// Error cases:
    /// - as for `apply_book`
    fn apply(&mut self, kind: Option<&str>, data: &str, now: Millis) -> Result<Vec<Book>, String> {
        let books: Vec<BookData> = serde_json::from_str(data).map_err(|e| format!("bad book: {e}"))?;
        let tops: Vec<Option<Book>> =
            books.into_iter().map(|book| self.apply_book(kind, book, now)).collect::<Result<_, _>>()?;
        Ok(tops.into_iter().flatten().collect())
    }

    /// Applies one symbol's snapshot or update, then checks the result against its checksum.
    /// Error cases:
    /// - the symbol is not one the connection subscribed to, the message type is unknown, or an
    ///   update arrives for a symbol with no snapshot
    /// - a price or size is not a plain decimal, a price is out of range, or a size is negative
    /// - the kept book's checksum differs from the venue's
    /// - the timestamp is not RFC 3339, or is implausible at `now` (see `check_time`)
    fn apply_book(&mut self, kind: Option<&str>, book: BookData, now: Millis) -> Result<Option<Book>, String> {
        let BookData { symbol, bids, asks, checksum, timestamp } = book;
        if !self.symbols.contains(&symbol) {
            return Err(format!("{symbol} not subscribed"));
        }
        let depth = match kind {
            Some("snapshot") => {
                let depth = self.depths.entry(symbol.clone()).or_default();
                *depth = Depth::default();
                depth
            }
            Some("update") => self.depths.get_mut(&symbol).ok_or_else(|| format!("{symbol} update before snapshot"))?,
            other => return Err(format!("unknown book message type {other:?}")),
        };
        apply_levels(&mut depth.bids, bids, &symbol)?;
        apply_levels(&mut depth.asks, asks, &symbol)?;
        depth.truncate();
        let kept = depth.checksum();
        if kept != checksum {
            return Err(format!("{symbol} checksum mismatch: the kept book gives {kept}, the venue sent {checksum}"));
        }
        let ts = DateTime::parse_from_rfc3339(timestamp)
            .map_err(|e| format!("bad timestamp {timestamp}: {e}"))?
            .timestamp_millis();
        check_time(&symbol, ts, now)?;
        let top = depth.bids.last_key_value().zip(depth.asks.first_key_value());
        Ok(top.map(|((bid, _), (ask, _))| Book { symbol, bid: *bid, ask: *ask, ts }))
    }
}

impl Depth {
    /// Keeps the best `DEPTH` levels a side, as Kraken's book does.
    fn truncate(&mut self) {
        while self.bids.len() > DEPTH {
            self.bids.pop_first();
        }
        while self.asks.len() > DEPTH {
            self.asks.pop_last();
        }
    }

    /// The checksum Kraken sends for this book.
    fn checksum(&self) -> u32 {
        let asks = self.asks.values();
        let bids = self.bids.values().rev();
        checksum(asks.chain(bids).map(|level| (level.price.as_str(), level.qty.as_str())))
    }
}

/// The subscribe message for the books of `symbols`.
pub(super) fn subscriptions(symbols: &[&str]) -> Vec<String> {
    let params = json!({ "channel": "book", "symbol": symbols, "depth": DEPTH });
    vec![json!({ "method": "subscribe", "params": params }).to_string()]
}

/// Kraken's book checksum: the CRC32 of each level's price then size, as written with the decimal
/// point and leading zeros removed, over the asks from the best up, then the bids from the best
/// down.
pub(super) fn checksum<'a>(levels: impl Iterator<Item = (&'a str, &'a str)>) -> u32 {
    let mut hasher = Hasher::new();
    for (price, qty) in levels {
        for text in [price, qty] {
            hasher.update(text.replace('.', "").trim_start_matches('0').as_bytes());
        }
    }
    hasher.finalize()
}

/// Sets or removes one side's levels from an update; a size of zero removes the level.
/// Error cases:
/// - a price or size is not a plain decimal, a price is out of range, or a size is negative
fn apply_levels(levels: &mut BTreeMap<Fixed, Level>, updates: Vec<RawLevel>, symbol: &str) -> Result<(), String> {
    for RawLevel { price, qty } in updates {
        let (price_text, qty_text) = (price.get(), qty.get());
        let price: Fixed = price_text.parse().map_err(|e| format!("{symbol} bad price: {e}"))?;
        let qty: Fixed = qty_text.parse().map_err(|e| format!("{symbol} bad size: {e}"))?;
        if !price.is_valid_input() || qty.is_negative() {
            return Err(format!("{symbol} level {price_text} x {qty_text} out of range"));
        }
        if qty.is_zero() {
            levels.remove(&price);
        } else {
            levels.insert(price, Level { price: price_text.to_owned(), qty: qty_text.to_owned() });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
