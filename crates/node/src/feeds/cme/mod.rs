//! CME Group's real-time WebSocket API (CME Market Data Over WebSocket). The handshake carries an
//! OAuth bearer token (`venue::auth`), and CME answers with its login status. Once authenticated, a
//! connection subscribes to the top of book (`TOB`) of each product it quotes, outrights only. It
//! names no period: CME refuses a whole subscription for one entry it cannot take, and the config
//! keeps a contract after it expires, so a period CME no longer lists would otherwise cost every
//! contract of its product. A `TOB` message restates the one-deep book of each instrument it carries
//! whole, at most every 500 ms per instrument, so the adapter keeps no state. It passes over the
//! instruments of a product that no symbol names, and the contracts of one that no symbol does,
//! but refuses a future of a named product with no `YYYYMM` period. Prices and sizes are decimal strings, and a book
//! is stamped with the message's `sentTime`, when CME's gateway sent it. CME sends a `HEARTBEAT`
//! every 10 seconds while nothing else is sent.
//!
//! A symbol names a contract by CME's product code and period, `<product>:<YYYYMM>` (`ES:202612`),
//! since a contract code's one-digit year (`ESZ6`) does not fix the decade. The config refuses a
//! symbol that does not name the contract it quotes. CME's pages spell the login status two ways,
//! and write a payload as an object in one place and an array in another; each documented form is
//! read, a status array only with one status, and anything else is refused.
//!
//! No text CME sends reaches a log line, an alert or an error: a refusal names a fixed reason, and
//! of what CME sent, only a value from a list CME documents (a message or product type, an error
//! code) and numbers. A message could otherwise carry a credential or token in some encoding.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::DateTime;
use engine_core::Fixed;
use engine_core::types::Millis;
use serde::Deserialize;
use serde_json::json;

use crate::feeds::venue::{Book, Frame, Refusal, check_size};

/// How often CME sends a `HEARTBEAT` while it sends nothing else.
pub(super) const HEARTBEAT: Duration = Duration::from_secs(10);
/// The futures month codes, January to December.
const MONTH_CODES: [char; 12] = ['F', 'G', 'H', 'J', 'K', 'M', 'N', 'Q', 'U', 'V', 'X', 'Z'];
/// The message types CME documents that a connection does not take.
const OTHER_TYPES: [&str; 4] = ["TRD", "STAT", "SUBSCRIBE", "UNSUBSCRIBE"];
/// The product types CME documents.
const PRODUCT_TYPES: [&str; 3] = ["FUT", "OOF", "MLEG"];
/// The error codes CME documents for a refused subscription.
const ERROR_CODES: [&str; 3] = ["ERROR_400", "ERROR_401", "ERROR_403"];

/// One connection's subscription, sent once CME accepts the login, and the symbols it quotes.
pub(super) struct Session {
    symbols: BTreeSet<String>,
    /// The product codes of `symbols`, which the subscription names whole.
    products: BTreeSet<String>,
}

/// One message: its header, and the payload or errors its type carries.
#[derive(Deserialize)]
struct Message {
    header: Header,
    payload: Option<serde_json::Value>,
    #[serde(default)]
    errors: Vec<SubscriptionError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    message_type: String,
    sent_time: Option<String>,
}

/// A status, of a login or a subscription.
#[derive(Deserialize)]
struct Status {
    status: String,
}

/// What a refused subscription names: its code (its message is not read).
#[derive(Debug, Deserialize)]
struct SubscriptionError {
    code: String,
}

/// One instrument's top of book.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Top {
    instrument: Instrument,
    bid_level: Vec<Level>,
    ask_level: Vec<Level>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Instrument {
    product_code: String,
    period_code: String,
    product_type: String,
    symbol: String,
}

/// A price level: its price and quantity.
#[derive(Deserialize)]
struct Level {
    price: Fixed,
    qty: Fixed,
}

/// One payload item, or several.
#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl Session {
    /// A session subscribing to `symbols`, each `<product>:<YYYYMM>`.
    pub(super) fn new(symbols: &[&str]) -> Self {
        let products = symbols
            .iter()
            .map(|symbol| parse_symbol(symbol).expect("the config load refused any other symbol").0.to_owned())
            .collect();
        Session { symbols: symbols.iter().map(|s| (*s).to_owned()).collect(), products }
    }

    /// The subscription to the top of book of every product a symbol names, outrights only.
    fn subscription(&self) -> String {
        let subscriptions: Vec<_> = self
            .products
            .iter()
            .map(|product| json!({ "productType": "FUT", "productCode": product, "spreadReportTypes": ["OUTRIGHT"] }))
            .collect();
        let payload = json!({ "subscriptionMessageTypes": ["TOB"], "subscriptions": subscriptions });
        json!({ "header": { "messageType": "SUBSCRIBE", "version": "1.0" }, "payload": payload }).to_string()
    }

    /// Reads one message: the subscription to send once the login is accepted, its acceptance, or
    /// the books a top of book carries for the symbols this session quotes.
    /// Error cases:
    /// - a message that does not parse, of an unknown type, or a refused subscription, or a top of
    ///   book with a price, size, instrument or time that does not parse or check (the connection
    ///   carries on)
    /// - a refused login, a login status other than authenticated (CME ends a session with one), or
    ///   a subscription status other than subscribed (the connection ends and retries)
    pub(super) fn read(&mut self, text: &str) -> Result<Frame, Refusal> {
        let Message { header, payload, errors } = serde_json::from_str(text).map_err(|e| {
            Refusal::Message(format!(
                "unparseable message ({:?} at line {}, column {})",
                e.classify(),
                e.line(),
                e.column()
            ))
        })?;
        match header.message_type.as_str() {
            "TOB" => {
                let books = books(header.sent_time.as_deref(), payload, &self.symbols, &self.products);
                books.map(Frame::Books).map_err(Refusal::Message)
            }
            "HEARTBEAT" => Ok(Frame::Control),
            "LOGIN_STATUS" | "LOGINSTATUS" => match status(payload).as_deref() {
                Some("AUTHENTICATED" | "authenticated") => Ok(Frame::Reply(vec![self.subscription()])),
                Some(_) => Err(Refusal::Connection("login status not authenticated".into())),
                None => Err(Refusal::Connection("login status without one status".into())),
            },
            "LOGINERR" => Err(Refusal::Connection("login refused".into())),
            "SUBSCRIPTION_STATUS" => match status(payload).as_deref() {
                Some("SUBSCRIBED") => Ok(Frame::Subscribed),
                Some(_) => Err(Refusal::Connection("subscription status not subscribed".into())),
                None => Err(Refusal::Connection("subscription status without one status".into())),
            },
            "SUBSCRIPTION_ERROR" => {
                let codes: Vec<&str> = errors
                    .iter()
                    .map(|e| documented(&e.code, &ERROR_CODES).unwrap_or("an undocumented code"))
                    .collect();
                Err(Refusal::Message(format!("subscription refused: {}", codes.join(", "))))
            }
            other => Err(Refusal::Message(match documented(other, &OTHER_TYPES) {
                Some(kind) => format!("unexpected message type {kind}"),
                None => "a message of an undocumented type".into(),
            })),
        }
    }
}

/// The status a login or subscription status carries, in a payload object or an array of one: none
/// when it carries none, or several, whose precedence CME does not document.
fn status(payload: Option<serde_json::Value>) -> Option<String> {
    match serde_json::from_value::<OneOrMany<Status>>(payload?).ok()? {
        OneOrMany::One(status) => Some(status.status),
        OneOrMany::Many(statuses) => match <[Status; 1]>::try_from(statuses) {
            Ok([status]) => Some(status.status),
            Err(_) => None,
        },
    }
}

/// The books a top of book message sent at `sent` carries for `symbols`, one per instrument quoting
/// both sides, as `<product>:<period>`, stamped `sent`.
/// Error cases:
/// - no `sentTime`, or one that is not RFC 3339, or a payload that does not parse
/// - a future of a product of `products` whose period is not `YYYYMM`, which CME's field table
///   promises (its own message example sends none)
/// - an instrument of `symbols` that is not a future, or not its outright contract (a futures
///   spread's period is its first leg's), or named twice, or a side deeper than one level, or a size
///   out of range
fn books(
    sent: Option<&str>,
    payload: Option<serde_json::Value>,
    symbols: &BTreeSet<String>,
    products: &BTreeSet<String>,
) -> Result<Vec<Book>, String> {
    let sent = sent.ok_or("top of book without a sentTime")?;
    let ts: Millis =
        DateTime::parse_from_rfc3339(sent).map_err(|_| "a sentTime that is not RFC 3339")?.timestamp_millis();
    let payload = payload.ok_or("top of book without a payload")?;
    let tops =
        match serde_json::from_value(payload).map_err(|e| format!("unparseable top of book ({:?})", e.classify()))? {
            OneOrMany::One(top) => vec![top],
            OneOrMany::Many(tops) => tops,
        };
    let (mut books, mut read) = (Vec::new(), BTreeSet::new());
    for Top { instrument, bid_level, ask_level } in tops {
        // An instrument of a product no symbol names is not this session's.
        if !products.contains(&instrument.product_code) {
            continue;
        }
        let symbol = format!("{}:{}", instrument.product_code, instrument.period_code);
        if !symbols.contains(&symbol) {
            // A future the session never named is another contract of a product it subscribed to
            // whole, if CME gives it a period.
            if instrument.product_type == "FUT" && parse_symbol(&symbol).is_err() {
                return Err(format!("a future of {} with a periodCode that is not YYYYMM", instrument.product_code));
            }
            continue;
        }
        if instrument.product_type != "FUT" {
            let kind = documented(&instrument.product_type, &PRODUCT_TYPES).unwrap_or("an undocumented");
            return Err(format!("{symbol} is quoted as a {kind} instrument, not a future"));
        }
        if !is_outright(&instrument.symbol, &instrument.product_code, &instrument.period_code) {
            return Err(format!("{symbol} is quoted under an instrument other than its outright contract"));
        }
        if !read.insert(symbol.clone()) {
            return Err(format!("{symbol} named twice"));
        }
        let mut top = Vec::new();
        for (side, levels) in [("bid", &bid_level), ("ask", &ask_level)] {
            match levels.as_slice() {
                [] => {}
                [level] => {
                    check_size(&symbol, side, level.qty)?;
                    top.push(level.price);
                }
                _ => return Err(format!("{symbol} {side} deeper than one level")),
            }
        }
        // A side with no level quotes nothing, as before a contract's first order.
        if let [bid, ask] = top[..] {
            books.push(Book { symbol, bid, ask, ts });
        }
    }
    Ok(books)
}

/// The product code and period a symbol names, `<product>:<YYYYMM>`.
/// Error cases:
/// - no `:`, a product code that is not 1 to 8 uppercase letters and digits, or a period that is
///   not six digits ending in a month from 01 to 12
pub(super) fn parse_symbol(symbol: &str) -> Result<(&str, &str), String> {
    let (product, period) = symbol.split_once(':').ok_or("not <product>:<YYYYMM>")?;
    let code = |b: u8| b.is_ascii_uppercase() || b.is_ascii_digit();
    if product.is_empty() || product.len() > 8 || !product.bytes().all(code) {
        return Err(format!("product code {product:?} is not 1 to 8 uppercase letters and digits"));
    }
    let month = period.get(4..).and_then(|month| month.parse::<usize>().ok());
    if period.len() != 6 || !period.bytes().all(|b| b.is_ascii_digit()) || !month.is_some_and(|m| (1..=12).contains(&m))
    {
        return Err(format!("period {period:?} is not YYYYMM"));
    }
    Ok((product, period))
}

/// `value` as one of `known`, the values CME documents for it, so a reason can name it: none for a
/// value CME does not document.
fn documented(value: &str, known: &[&'static str]) -> Option<&'static str> {
    known.iter().copied().find(|known| *known == value)
}

/// Checks a CME symbol names `contract`, the listing's name for it: its product code, then the
/// period's month code, then the last digit of its year (`ES:202612` names `ESZ6`).
/// Error cases:
/// - the symbol does not parse (see `parse_symbol`), or names no contract, or another one
pub(crate) fn check_symbol(symbol: &str, contract: Option<&str>) -> Result<(), String> {
    let (product, period) = parse_symbol(symbol)?;
    let contract = contract.ok_or("a CME symbol names the contract it quotes")?;
    let named = globex(product, period, 1);
    if contract != named {
        return Err(format!("names {named}, not contract {contract}"));
    }
    Ok(())
}

/// Whether `instrument` is the outright contract of `product` in `period`, as CME Globex names it:
/// the product code, the month code, and the year's last digit, or its last two for a contract a
/// decade out, as CME's message example's `CLG34` has.
fn is_outright(instrument: &str, product: &str, period: &str) -> bool {
    parse_symbol(&format!("{product}:{period}")).is_ok()
        && [1, 2].iter().any(|&digits| instrument == globex(product, period, digits))
}

/// The Globex name of the contract of `product` in `period`, a `YYYYMM` `parse_symbol` takes, with
/// the last `digits` of its year.
pub(super) fn globex(product: &str, period: &str, digits: usize) -> String {
    let month = MONTH_CODES[period[4..].parse::<usize>().expect("parse_symbol checked the month") - 1];
    format!("{product}{month}{}", &period[4 - digits..4])
}

#[cfg(test)]
mod tests;
