//! Databento's live gateway, one dataset per connection, read as lines over plain TCP. The gateway
//! greets the client and sends a challenge; the client answers with the SHA-256 of the challenge
//! and its API key, so the key never crosses the network, then subscribes to each symbol's top of
//! book (`mbp-1`) in JSON and starts the session. Records follow, one JSON object a line: symbol
//! mappings that name each instrument id, top-of-book records stamped with the exchange's event
//! time in nanoseconds, heartbeats, and errors. Prices are integers in units of 10^-9, the scale
//! `Fixed` keeps, written as strings.

use std::collections::BTreeMap;

use engine_core::Fixed;
use engine_core::types::Millis;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::feeds::venue::{Book, Frame, Refusal, check_size};

/// Record types, as the `rtype` of a record's header gives them.
const MBP_1: u8 = 0x01;
const ERROR: u8 = 0x15;
const SYMBOL_MAPPING: u8 = 0x16;
const SYSTEM: u8 = 0x17;
/// A price field with no price: the side of the book is empty.
const UNDEF_PRICE: i64 = i64::MAX;
/// Characters of the API key that name its bucket in the challenge response.
const BUCKET_CHARS: usize = 5;
const NANOS_PER_MILLI: u64 = 1_000_000;

/// One connection's handshake, and the symbol each instrument id names.
pub(super) struct Session {
    key: String,
    dataset: String,
    symbols: Vec<String>,
    /// Whether the session has started, so lines are records rather than control messages.
    streaming: bool,
    /// The subscribed symbol each instrument id names, at most one id a symbol.
    instruments: BTreeMap<u32, String>,
}

/// A record, or the metadata the gateway may send ahead of them.
#[derive(Deserialize)]
struct Record {
    hd: Option<Header>,
    stype_in_symbol: Option<String>,
    #[serde(default)]
    levels: Vec<Level>,
    err: Option<String>,
    dataset: Option<String>,
}

#[derive(Deserialize)]
struct Header {
    ts_event: String,
    rtype: u8,
    instrument_id: u32,
}

/// The top of book in an `mbp-1` record.
#[derive(Deserialize)]
struct Level {
    bid_px: String,
    ask_px: String,
    bid_sz: u32,
    ask_sz: u32,
}

impl Session {
    /// A session authenticating with `key` for `dataset` and subscribing to `symbols`.
    pub(super) fn new(key: &str, dataset: &str, symbols: &[&str]) -> Self {
        Session {
            key: key.to_owned(),
            dataset: dataset.to_owned(),
            symbols: symbols.iter().map(|s| (*s).to_owned()).collect(),
            streaming: false,
            instruments: BTreeMap::new(),
        }
    }

    /// Reads one line: a control message before the session starts, a record after.
    /// Error cases:
    /// - as for `control` and `record`
    pub(super) fn read(&mut self, line: &str) -> Result<Frame, Refusal> {
        if self.streaming { self.record(line).map_err(Refusal::Message) } else { self.control(line) }
    }

    /// Reads a control message: the greeting, the challenge, answered with the authentication, or
    /// the authentication's result, answered with the subscription and the session start.
    /// Error cases:
    /// - a control message the protocol does not define (the connection carries on)
    /// - a failed authentication (the connection ends and retries)
    fn control(&mut self, line: &str) -> Result<Frame, Refusal> {
        let fields: BTreeMap<&str, &str> = line.split('|').filter_map(|field| field.split_once('=')).collect();
        if fields.contains_key("lsg_version") {
            return Ok(Frame::Control);
        }
        if let Some(challenge) = fields.get("cram") {
            return Ok(Frame::Reply(vec![self.authentication(challenge)]));
        }
        match fields.get("success") {
            Some(&"1") => {
                self.streaming = true;
                let subscription = format!("schema=mbp-1|stype_in=raw_symbol|symbols={}", self.symbols.join(","));
                Ok(Frame::Reply(vec![subscription, "start_session".into()]))
            }
            Some(_) => {
                Err(Refusal::Connection(format!("authentication failed: {}", fields.get("error").unwrap_or(&""))))
            }
            None => Err(Refusal::Message("unexpected control message".into())),
        }
    }

    /// The authentication answering `challenge`: the SHA-256 of the challenge and the key, the
    /// key's bucket, and the dataset and encoding the session streams.
    fn authentication(&self, challenge: &str) -> String {
        let digest = Sha256::digest(format!("{challenge}|{}", self.key).as_bytes());
        let bucket_from = self.key.char_indices().rev().nth(BUCKET_CHARS - 1).map_or(0, |(at, _)| at);
        let bucket = &self.key[bucket_from..];
        format!("auth={}-{bucket}|dataset={}|encoding=json|ts_out=0", hex::encode(digest), self.dataset)
    }

    /// Reads a record: a symbol mapping, a top of book, a heartbeat, or an error.
    /// Error cases:
    /// - a line that is not a record, an unknown record type, or an error record
    /// - a mapping for a symbol not subscribed, or a top of book for an instrument no mapping named
    /// - a price, size, or time that does not parse, or a size out of range
    fn record(&mut self, line: &str) -> Result<Frame, String> {
        let record: Record = serde_json::from_str(line).map_err(|e| format!("unparseable record: {e}"))?;
        let Some(header) = &record.hd else {
            return match record.dataset {
                Some(_) => Ok(Frame::Control),
                None => Err("a line that is neither a record nor metadata".into()),
            };
        };
        match header.rtype {
            SYMBOL_MAPPING => {
                let symbol = record.stype_in_symbol.ok_or("symbol mapping without a symbol")?;
                if !self.symbols.contains(&symbol) {
                    return Err(format!("mapping for {symbol}, which is not subscribed"));
                }
                // One id a symbol, so remappings cannot grow the table.
                self.instruments.retain(|_, mapped| *mapped != symbol);
                self.instruments.insert(header.instrument_id, symbol);
                Ok(Frame::Control)
            }
            MBP_1 => {
                let symbol = self.instruments.get(&header.instrument_id).ok_or_else(|| {
                    format!("top of book for instrument {}, which no mapping names", header.instrument_id)
                })?;
                let level = record.levels.first().ok_or_else(|| format!("{symbol} top of book without a level"))?;
                Ok(Frame::Books(top(symbol, level, &header.ts_event)?.into_iter().collect()))
            }
            SYSTEM => Ok(Frame::Control),
            ERROR => Err(format!("venue error: {}", record.err.unwrap_or_default())),
            other => Err(format!("unexpected record type {other:#04x}")),
        }
    }
}

/// The book an `mbp-1` level gives, or none while a side is empty.
/// Error cases:
/// - a price or time that does not parse, or a size out of range on a side with a price
fn top(symbol: &str, level: &Level, ts_event: &str) -> Result<Option<Book>, String> {
    let price = |text: &str| text.parse::<i64>().map_err(|e| format!("{symbol} bad price {text}: {e}"));
    let (bid, ask) = (price(&level.bid_px)?, price(&level.ask_px)?);
    if bid == UNDEF_PRICE || ask == UNDEF_PRICE {
        return Ok(None);
    }
    check_size(symbol, "bid", Fixed::from_int(level.bid_sz.into()))?;
    check_size(symbol, "ask", Fixed::from_int(level.ask_sz.into()))?;
    let nanos: u64 = ts_event.parse().map_err(|e| format!("{symbol} bad ts_event {ts_event}: {e}"))?;
    let ts =
        Millis::try_from(nanos / NANOS_PER_MILLI).map_err(|_| format!("{symbol} ts_event {ts_event} out of range"))?;
    let bid = Fixed::from_raw(bid.into());
    let ask = Fixed::from_raw(ask.into());
    Ok(Some(Book { symbol: symbol.to_owned(), bid, ask, ts }))
}

#[cfg(test)]
mod tests;
