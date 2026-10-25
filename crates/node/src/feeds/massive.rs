//! Massive (formerly Polygon.io). Each asset class has its own WebSocket feed: `/forex` sends FX
//! quotes on the `C` channel, `/futures` and `/stocks` send quotes on the `Q` channel. A connection
//! authenticates with the API key, subscribes once Massive accepts it, then receives arrays of
//! events, each quote stamped with Massive's time in milliseconds.
//!
//! Prices arrive as JSON numbers. They are read from the raw number text, never through a float.

use engine_core::Fixed;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_tungstenite::tungstenite::http::Uri;

use crate::feeds::venue::{Book, Frame, Refusal, check_size};

/// One connection's handshake: the key it authenticates with and the subscription that follows.
pub(super) struct Session {
    key: String,
    /// `C` on the FX feed, `Q` on the futures and stocks feeds.
    channel: &'static str,
    symbols: Vec<String>,
}

/// One event of a message: a status or a quote, by `ev`.
#[derive(Deserialize)]
struct Event<'a> {
    ev: &'a str,
    status: Option<&'a str>,
    message: Option<String>,
    /// An FX quote's pair, written `EUR/USD`.
    p: Option<&'a str>,
    /// A futures or stock quote's symbol.
    sym: Option<&'a str>,
    /// An FX quote's bid and ask.
    #[serde(borrow)]
    b: Option<&'a RawValue>,
    #[serde(borrow)]
    a: Option<&'a RawValue>,
    /// A futures or stock quote's bid and ask, and their sizes.
    #[serde(borrow)]
    bp: Option<&'a RawValue>,
    #[serde(borrow)]
    ap: Option<&'a RawValue>,
    #[serde(borrow)]
    bs: Option<&'a RawValue>,
    #[serde(borrow, rename = "as")]
    size_ask: Option<&'a RawValue>,
    t: Option<i64>,
}

impl Session {
    /// A session on the feed at `url`, authenticating with `key` and subscribing to `symbols`.
    pub(super) fn new(url: &str, key: &str, symbols: &[&str]) -> Self {
        let path = url.parse::<Uri>().map(|uri| uri.path().to_owned()).unwrap_or_default();
        let channel = if path == "/forex" { "C" } else { "Q" };
        Session { key: key.to_owned(), channel, symbols: symbols.iter().map(|s| (*s).to_owned()).collect() }
    }

    /// The authentication message a connection opens with.
    pub(super) fn opening(&self) -> Vec<String> {
        vec![json!({ "action": "auth", "params": self.key }).to_string()]
    }

    /// The subscription to every symbol on this feed's channel.
    fn subscription(&self) -> String {
        let params: Vec<String> = self.symbols.iter().map(|symbol| format!("{}.{symbol}", self.channel)).collect();
        json!({ "action": "subscribe", "params": params.join(",") }).to_string()
    }

    /// Reads one message: the subscription to send once authenticated, or the books it carries.
    /// An FX pair is read as the subscription names it, `EUR-USD`.
    /// Error cases:
    /// - a message that is not an array of events, an unknown event or status, or a quote whose
    ///   price, size, or time does not parse (the connection carries on)
    /// - a failed authentication (the connection ends and retries)
    pub(super) fn read(&mut self, text: &str) -> Result<Frame, Refusal> {
        let events: Vec<Event> =
            serde_json::from_str(text).map_err(|e| Refusal::Message(format!("unparseable message: {e}")))?;
        let mut books = Vec::new();
        for event in events {
            match (event.ev, event.status) {
                ("status", Some("auth_success")) => return Ok(Frame::Reply(vec![self.subscription()])),
                ("status", Some("connected" | "success")) => {}
                ("status", Some("auth_failed")) => {
                    return Err(Refusal::Connection(format!(
                        "authentication failed: {}",
                        event.message.unwrap_or_default()
                    )));
                }
                ("status", status) => {
                    let message = event.message.unwrap_or_default();
                    return Err(Refusal::Message(format!("venue status {status:?}: {message}")));
                }
                ("C" | "Q", _) => books.extend(quote(&event).map_err(Refusal::Message)?),
                (other, _) => return Err(Refusal::Message(format!("unexpected event {other}"))),
            }
        }
        Ok(Frame::Books(books))
    }
}

/// The book an FX (`C`) or futures and stock (`Q`) quote carries, or none while a side is missing,
/// as around the pre-market open.
/// Error cases:
/// - no symbol or time, or a price or size that does not parse, or a size out of range
fn quote(event: &Event) -> Result<Option<Book>, String> {
    let (symbol, bid, ask) = match event.ev {
        "C" => (event.p.map(|pair| pair.replace('/', "-")), event.b, event.a),
        _ => (event.sym.map(str::to_owned), event.bp, event.ap),
    };
    let symbol = symbol.ok_or("quote without a symbol")?;
    let ts = event.t.ok_or_else(|| format!("{symbol} quote without a time"))?;
    for (side, size) in [("bid", event.bs), ("ask", event.size_ask)] {
        if let Some(size) = size {
            check_size(&symbol, side, number(&symbol, size)?)?;
        }
    }
    let (Some(bid), Some(ask)) = (bid, ask) else { return Ok(None) };
    Ok(Some(Book { bid: number(&symbol, bid)?, ask: number(&symbol, ask)?, symbol, ts }))
}

/// A JSON number read exactly as its text.
/// Error cases:
/// - the text is not a decimal `Fixed` reads, such as one in exponent form
fn number(symbol: &str, raw: &RawValue) -> Result<Fixed, String> {
    raw.get().parse().map_err(|e| format!("{symbol} bad number {}: {e}", raw.get()))
}

#[cfg(test)]
mod tests;
