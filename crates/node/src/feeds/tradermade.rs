//! TraderMade's streaming feed. A connection logs in with the API key, subscribes to each pair's
//! `QUOTE` stream once the login is accepted, then receives one quote per message: bid, ask, their
//! sizes, and TraderMade's time, `YYYYMMDD-HH:MM:SS.mmm` in UTC. Prices and sizes are strings.

use chrono::NaiveDateTime;
use engine_core::Fixed;
use serde::Deserialize;
use serde_json::json;

use crate::feeds::venue::{Book, Frame, Refusal, check_size};

/// TraderMade's time format.
const TIME_FORMAT: &str = "%Y%m%d-%H:%M:%S%.3f";

/// One connection's handshake: the key it logs in with and the subscription that follows.
pub(super) struct Session {
    key: String,
    symbols: Vec<String>,
}

/// A control message, by `type`, or a quote, with `t` set to `QUOTE`.
#[derive(Deserialize)]
struct Message<'a> {
    #[serde(rename = "type")]
    kind: Option<&'a str>,
    t: Option<&'a str>,
    #[serde(default)]
    denied: Vec<String>,
    #[serde(default)]
    invalid: Vec<String>,
    s: Option<String>,
    b: Option<Fixed>,
    a: Option<Fixed>,
    bv: Option<Fixed>,
    av: Option<Fixed>,
    ts: Option<&'a str>,
}

impl Session {
    /// A session logging in with `key` and subscribing to `symbols`, as TraderMade names its pairs
    /// (`EURUSD`).
    pub(super) fn new(key: &str, symbols: &[&str]) -> Self {
        Session { key: key.to_owned(), symbols: symbols.iter().map(|s| (*s).to_owned()).collect() }
    }

    /// The login a connection opens with.
    pub(super) fn opening(&self) -> Vec<String> {
        vec![json!({ "action": "login", "key": self.key, "fmt": "JSON" }).to_string()]
    }

    /// The subscription to every symbol's quotes.
    fn subscription(&self) -> String {
        let symbols: Vec<String> = self.symbols.iter().map(|symbol| format!("{symbol}:QUOTE")).collect();
        json!({ "action": "subscribe", "symbols": symbols }).to_string()
    }

    /// Reads one message: the subscription to send once logged in, or the book a quote carries.
    /// Error cases:
    /// - an unknown message, a subscription TraderMade denied in part, or a quote missing a field
    ///   or with one that does not parse, or a size out of range (the connection carries on)
    /// - a refused login (the connection ends and retries)
    pub(super) fn read(&mut self, text: &str) -> Result<Frame, Refusal> {
        let message: Message =
            serde_json::from_str(text).map_err(|e| Refusal::Message(format!("unparseable message: {e}")))?;
        match (message.kind, message.t) {
            (Some("login_ok"), _) => Ok(Frame::Reply(vec![self.subscription()])),
            (Some("login_reject"), _) => Err(Refusal::Connection("login refused".into())),
            (Some("sub_ack"), _) if message.denied.is_empty() && message.invalid.is_empty() => Ok(Frame::Control),
            (Some("sub_ack"), _) => Err(Refusal::Message(format!(
                "subscription denied {:?}, invalid {:?}",
                message.denied, message.invalid
            ))),
            (Some("unsub_ack"), _) => Ok(Frame::Control),
            (None, Some("QUOTE")) => quote(message).map(|book| Frame::Books(vec![book])).map_err(Refusal::Message),
            (kind, t) => Err(Refusal::Message(format!("unexpected message type {kind:?}, t {t:?}"))),
        }
    }
}

/// The book a quote carries.
/// Error cases:
/// - a missing field, a time that does not parse, or a size out of range
fn quote(message: Message) -> Result<Book, String> {
    let Message { s, b, a, bv, av, ts, .. } = message;
    let symbol = s.ok_or("quote without a symbol")?;
    let (Some(bid), Some(ask), Some(bid_size), Some(ask_size), Some(at)) = (b, a, bv, av, ts) else {
        return Err(format!("{symbol} quote missing a field"));
    };
    check_size(&symbol, "bid", bid_size)?;
    check_size(&symbol, "ask", ask_size)?;
    let ts = NaiveDateTime::parse_from_str(at, TIME_FORMAT)
        .map_err(|e| format!("{symbol} bad time {at}: {e}"))?
        .and_utc()
        .timestamp_millis();
    Ok(Book { symbol, bid, ask, ts })
}

#[cfg(test)]
mod tests {
    use super::*;

    // From TraderMade's streaming documentation.
    const QUOTE: &str = r#"{"a":"1.162720000","av":"100000","b":"1.162700000","bv":"100000","s":"EURUSD","t":"QUOTE","ts":"20260515-12:36:35.588"}"#;
    const LOGIN_OK: &str =
        r#"{"type":"login_ok","key":"tm-key","fmt":"JSON","symbol_limit":54,"cfds":true,"trader_ladder":false}"#;
    const SUB_ACK: &str =
        r#"{"accepted":["EURUSD:QUOTE","GBPUSD:QUOTE"],"denied":[],"denied_reasons":{},"invalid":[],"type":"sub_ack"}"#;

    #[test]
    fn a_login_is_answered_with_the_subscription_and_then_quotes_give_books() {
        let mut session = Session::new("tm-key", &["EURUSD", "GBPUSD"]);
        let login: serde_json::Value = serde_json::from_str(&session.opening()[0]).unwrap();
        assert_eq!(login, json!({ "action": "login", "key": "tm-key", "fmt": "JSON" }));
        let Ok(Frame::Reply(reply)) = session.read(LOGIN_OK) else { panic!("no subscription") };
        let subscription: serde_json::Value = serde_json::from_str(&reply[0]).unwrap();
        assert_eq!(subscription, json!({ "action": "subscribe", "symbols": ["EURUSD:QUOTE", "GBPUSD:QUOTE"] }));
        assert_eq!(session.read(SUB_ACK), Ok(Frame::Control));

        // 2026-05-15T12:36:35.588Z.
        let book = Book {
            symbol: "EURUSD".into(),
            bid: "1.1627".parse().unwrap(),
            ask: "1.16272".parse().unwrap(),
            ts: 1_778_848_595_588,
        };
        assert_eq!(session.read(QUOTE), Ok(Frame::Books(vec![book])));
    }

    #[test]
    fn a_refused_login_ends_the_connection_and_a_denied_symbol_is_refused() {
        let mut session = Session::new("tm-key", &["EURUSD"]);
        let rejected = session.read(r#"{"type":"login_reject","key":"tm-key"}"#);
        assert_eq!(rejected, Err(Refusal::Connection("login refused".into())));
        let denied = SUB_ACK.replace(r#""denied":[]"#, r#""denied":["XXXUSD:QUOTE"]"#);
        assert!(matches!(session.read(&denied), Err(Refusal::Message(reason)) if reason.contains("XXXUSD")));
    }

    #[test]
    fn malformed_quotes_are_refused() {
        let cases = [
            QUOTE.replace(r#""b":"1.162700000","#, ""),
            QUOTE.replace("20260515-12:36:35.588", "2026-05-15T12:36:35Z"),
            QUOTE.replace(r#""a":"1.162720000""#, r#""a":"1,16272""#),
            QUOTE.replace(r#""bv":"100000""#, r#""bv":"0""#),
            QUOTE.replace(r#""t":"QUOTE""#, r#""t":"LADDER""#),
            QUOTE.replace(r#""s":"EURUSD","#, ""),
            "[]".to_owned(),
        ];
        for case in cases {
            let mut session = Session::new("tm-key", &["EURUSD"]);
            assert!(matches!(session.read(&case), Err(Refusal::Message(_))), "accepted {case}");
        }
    }
}
