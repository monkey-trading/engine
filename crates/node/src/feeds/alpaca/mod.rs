//! Alpaca's real-time stock data over WebSocket, on its consolidated feed (`/v2/sip`, of the Algo
//! Trader Plus plan): quotes from every US exchange, so a quote is the national best bid and offer,
//! not one venue's. A connection opens with a login message carrying the API key ID and secret key,
//! and once Alpaca answers `authenticated` it subscribes to the quotes of its symbols. Alpaca sends
//! arrays of messages typed by `T`: a control message (`success`, `error`, `subscription`) alone in
//! its array, and quotes (`q`) one or more to an array. A quote restates its symbol's best bid and
//! ask whole, so the adapter keeps no state, and each is read on its own: one it refuses costs only
//! itself, not the quotes batched with it. Prices and sizes are JSON numbers, read from their
//! text, never through a float, and a book is stamped with the quote's RFC 3339 time `t`, to the
//! millisecond. Alpaca does not document a one-sided quote, so a side whose price and size are both
//! zero is read as quoting nothing, and that quote gives no book.
//!
//! No text Alpaca sends reaches a log line, an alert or an error: a refusal names a fixed reason, and
//! of what Alpaca sent, only a value from a list Alpaca documents (a message type, an error code with
//! its meaning) and numbers, and a symbol only once it is one the connection subscribed to.

use std::borrow::Cow;
use std::collections::BTreeSet;

use chrono::DateTime;
use engine_core::Fixed;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;

use crate::feeds::venue::{Book, Frame, Refusal, check_size};

/// The control message types, each sent alone in its array.
const CONTROL_TYPES: [&str; 3] = ["success", "error", "subscription"];
/// The other message types Alpaca documents for its stock stream, none of which a connection
/// subscribes to: trades, bars, daily and updated bars, trading statuses, LULD bands, trade
/// corrections and cancels, and order imbalances.
const OTHER_TYPES: [&str; 9] = ["t", "b", "d", "u", "s", "l", "c", "x", "i"];
/// The error codes Alpaca documents, its meaning of each, and whether it ends the session: a failed,
/// missing, repeated or late login, a connection past the plan's limit, or a feed the plan does not
/// include. The others leave the connection up, a refused subscription among them, and a connection
/// that then quotes nothing is replaced as any is.
const ERRORS: [(i64, &str, bool); 11] = [
    (400, "invalid syntax", false),
    (401, "not authenticated", true),
    (402, "auth failed", true),
    (403, "already authenticated", true),
    (404, "auth timeout", true),
    (405, "symbol limit exceeded", false),
    (406, "connection limit exceeded", true),
    (407, "slow client", false),
    (409, "insufficient subscription", true),
    (410, "invalid subscribe action for this feed", false),
    (500, "internal error", false),
];

/// One connection's login and the quote subscription that follows it.
pub(super) struct Session {
    key_id: String,
    secret_key: String,
    symbols: BTreeSet<String>,
}

/// One message of an array: a control message or a quote, by `T`. Its strings are borrowed from the
/// message where they can be and owned where a JSON escape has to be decoded, so an escape in any
/// of them leaves the message readable.
#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow, rename = "T")]
    kind: Cow<'a, str>,
    /// A `success` message's status.
    #[serde(borrow)]
    msg: Option<Cow<'a, str>>,
    /// An `error` message's code.
    code: Option<i64>,
    /// The symbols a `subscription` message confirms quotes for.
    quotes: Option<Vec<String>>,
    /// A quote's symbol.
    #[serde(borrow, rename = "S")]
    symbol: Option<Cow<'a, str>>,
    /// A quote's bid and ask, and their sizes.
    #[serde(borrow)]
    bp: Option<&'a RawValue>,
    #[serde(borrow)]
    bs: Option<&'a RawValue>,
    #[serde(borrow)]
    ap: Option<&'a RawValue>,
    #[serde(borrow, rename = "as")]
    size_ask: Option<&'a RawValue>,
    /// A quote's time, RFC 3339 to the nanosecond.
    #[serde(borrow)]
    t: Option<Cow<'a, str>>,
}

impl Session {
    /// A session logging in as `key_id` with `secret_key` and subscribing to the quotes of `symbols`.
    pub(super) fn new(key_id: &str, secret_key: &str, symbols: &[&str]) -> Self {
        let symbols = symbols.iter().map(|symbol| (*symbol).to_owned()).collect();
        Session { key_id: key_id.to_owned(), secret_key: secret_key.to_owned(), symbols }
    }

    /// The login message a connection opens with.
    pub(super) fn opening(&self) -> Vec<String> {
        vec![json!({ "action": "auth", "key": self.key_id, "secret": self.secret_key }).to_string()]
    }

    /// The subscription to the quotes of every symbol.
    fn subscription(&self) -> String {
        json!({ "action": "subscribe", "quotes": self.symbols }).to_string()
    }

    /// Reads one message array: the subscription to send once the login is accepted, its
    /// confirmation, or the books its quotes carry.
    /// Error cases:
    /// - an array that does not parse, a lone message that does not, a `success` of an undocumented status, an error that leaves
    ///   the session up (a refused subscription among them) or of an undocumented code, or a
    ///   confirmation that leaves a symbol out (the connection carries on)
    /// - an error that ends the session (see `ERRORS`): the connection ends and retries
    ///
    /// Within an array of quotes, a control message batched with others, a message that does not parse, a
    /// message of a type other than a quote, or a quote that does not parse or check is refused alone, and the array's other
    /// books are read (`Frame::PartlyRefused`).
    pub(super) fn read(&mut self, text: &str) -> Result<Frame, Refusal> {
        let elements: Vec<&RawValue> = serde_json::from_str(text).map_err(|e| Refusal::Message(unparseable(&e)))?;
        // Each element is read on its own, so one that does not deserialize costs only itself.
        let messages: Vec<Result<Message, String>> =
            elements.iter().map(|raw| serde_json::from_str(raw.get()).map_err(|e| unparseable(&e))).collect();
        if let [message] = messages.as_slice() {
            let message = message.as_ref().map_err(|reason| Refusal::Message(reason.clone()))?;
            if CONTROL_TYPES.contains(&message.kind.as_ref()) {
                return self.control(message);
            }
        }
        let (mut books, mut refused) = (Vec::new(), Vec::new());
        for message in &messages {
            match message.as_ref().map_err(Clone::clone).and_then(|message| self.quote(message)) {
                Ok(book) => books.extend(book),
                Err(reason) => refused.push(reason),
            }
        }
        if refused.is_empty() {
            return Ok(Frame::Books(books));
        }
        Ok(Frame::PartlyRefused { books, refused })
    }

    /// Reads a control message sent alone in its array.
    fn control(&self, message: &Message) -> Result<Frame, Refusal> {
        match (message.kind.as_ref(), message.msg.as_deref()) {
            ("success", Some("connected")) => Ok(Frame::Control),
            ("success", Some("authenticated")) => Ok(Frame::Reply(vec![self.subscription()])),
            ("success", _) => Err(Refusal::Message("a success message of an undocumented status".into())),
            ("subscription", _) => self.confirmed(message.quotes.as_deref().unwrap_or_default()),
            // An `error`, the one other control type.
            _ => Err(refused(message.code)),
        }
    }

    /// Reads a subscription's confirmation, which lists every symbol the connection takes quotes for.
    /// Error cases:
    /// - a symbol of this session is not among `quotes`
    fn confirmed(&self, quotes: &[String]) -> Result<Frame, Refusal> {
        let confirmed = self.symbols.iter().filter(|symbol| quotes.contains(symbol)).count();
        if confirmed < self.symbols.len() {
            let reason = format!("the subscription confirms quotes for {confirmed} of {} symbols", self.symbols.len());
            return Err(Refusal::Message(reason));
        }
        Ok(Frame::Subscribed)
    }

    /// The book a quote carries for a symbol this session subscribed to, stamped with its time, or
    /// none while a side quotes nothing.
    /// Error cases:
    /// - a message other than a quote, or a quote for a symbol this session did not subscribe to
    /// - a quote without its time, or one that is not RFC 3339, or a side that does not check (see
    ///   `side`)
    fn quote(&self, message: &Message) -> Result<Option<Book>, String> {
        if message.kind != "q" {
            return Err(unexpected(&message.kind));
        }
        let symbol = message
            .symbol
            .as_deref()
            .and_then(|symbol| self.symbols.get(symbol))
            .ok_or("a quote for a symbol the connection did not subscribe to")?;
        let time = message.t.as_deref().ok_or_else(|| format!("{symbol} quote without a time"))?;
        let ts = DateTime::parse_from_rfc3339(time)
            .map_err(|_| format!("{symbol} quote time is not RFC 3339"))?
            .timestamp_millis();
        let bid = side(symbol, "bid", message.bp, message.bs)?;
        let ask = side(symbol, "ask", message.ap, message.size_ask)?;
        let (Some(bid), Some(ask)) = (bid, ask) else { return Ok(None) };
        Ok(Some(Book { symbol: symbol.clone(), bid, ask, ts }))
    }
}

/// A quote side's price, or none when its price and size are both zero.
/// Error cases:
/// - no price or size, a number that is not a decimal `Fixed` reads, such as one in exponent form,
///   or a size out of range
fn side(symbol: &str, side: &str, price: Option<&RawValue>, size: Option<&RawValue>) -> Result<Option<Fixed>, String> {
    let (Some(price), Some(size)) = (price, size) else {
        return Err(format!("{symbol} quote without its {side} price and size"));
    };
    let number = |raw: &RawValue, field: &str| {
        raw.get().parse::<Fixed>().map_err(|_| format!("{symbol} {side} {field} is not a decimal"))
    };
    let (price, size) = (number(price, "price")?, number(size, "size")?);
    if price == Fixed::ZERO && size == Fixed::ZERO {
        return Ok(None);
    }
    check_size(symbol, side, size)?;
    Ok(Some(price))
}

/// Why a message or one element of an array does not deserialize: its error's class and position,
/// never Alpaca's text.
fn unparseable(e: &serde_json::Error) -> String {
    format!("unparseable message ({:?} at line {}, column {})", e.classify(), e.line(), e.column())
}

/// The refusal an `error` with `code` gives: the connection ends for an error that ends the session,
/// and either way the code is named with Alpaca's meaning of it.
fn refused(code: Option<i64>) -> Refusal {
    let Some(code) = code else { return Refusal::Message("an error without a code".into()) };
    match ERRORS.iter().find(|(known, ..)| *known == code) {
        Some((_, meaning, true)) => Refusal::Connection(format!("error {code}: {meaning}")),
        Some((_, meaning, false)) => Refusal::Message(format!("error {code}: {meaning}")),
        None => Refusal::Message(format!("error {code}, a code Alpaca does not document")),
    }
}

/// Why a message among quotes is refused: a control message comes alone in its array, and the
/// connection subscribes to no other type.
fn unexpected(kind: &str) -> String {
    if let Some(control) = CONTROL_TYPES.iter().find(|known| **known == kind) {
        return format!("a {control} message batched with others");
    }
    match OTHER_TYPES.iter().find(|known| **known == kind) {
        Some(other) => format!("unexpected message type {other}"),
        None => "a message of an undocumented type".into(),
    }
}

#[cfg(test)]
mod tests;
