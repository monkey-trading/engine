use super::*;

use serde_json::Value;

/// The control messages of Alpaca's WebSocket documentation.
const CONNECTED: &str = r#"[{"T":"success","msg":"connected"}]"#;
const AUTHENTICATED: &str = r#"[{"T":"success","msg":"authenticated"}]"#;
const AUTH_FAILED: &str = r#"[{"T":"error","code":402,"msg":"auth failed"}]"#;
/// The documentation's subscription confirmation, for this test's symbols.
const SUBSCRIBED: &str = r#"[{"T":"subscription","trades":[],"quotes":["AAPL","MSFT"],"bars":[],"updatedBars":[],"dailyBars":[],"statuses":[],"lulds":[],"corrections":[],"cancelErrors":[]}]"#;
/// The documentation's quote example, for AAPL.
const QUOTE: &str = r#"{"T":"q","S":"AAPL","bx":"U","bp":87.66,"bs":1,"ax":"Q","ap":87.68,"as":4,"t":"2021-02-22T15:51:45.335689322Z","c":["R"],"z":"C"}"#;
/// `QUOTE`'s time, to the millisecond.
const QUOTE_TS: i64 = 1_614_009_105_335;
const KEY_ID: &str = "AKTESTKEYID0123456789";
const SECRET_KEY: &str = "test-secret-key-abcdefghijklmnopqrstuvwxyz";

#[test]
fn a_connection_logs_in_with_its_key_pair_then_subscribes_to_its_symbols_quotes() {
    let mut session = session();
    let login: Value = serde_json::from_str(&session.opening()[0]).unwrap();
    assert_eq!(login, json!({ "action": "auth", "key": KEY_ID, "secret": SECRET_KEY }));
    assert_eq!(session.read(CONNECTED), Ok(Frame::Control));
    let Ok(Frame::Reply(reply)) = session.read(AUTHENTICATED) else { panic!("no subscription") };
    let subscription: Value = serde_json::from_str(&reply[0]).unwrap();
    assert_eq!(subscription, json!({ "action": "subscribe", "quotes": ["AAPL", "MSFT"] }));
    assert_eq!(session.read(SUBSCRIBED), Ok(Frame::Subscribed));
}

#[test]
fn quotes_give_books_at_their_time_to_the_millisecond() {
    let mut session = session();
    let book = |symbol: &str, bid: &str, ask: &str| Book {
        symbol: symbol.into(),
        bid: bid.parse().unwrap(),
        ask: ask.parse().unwrap(),
        ts: QUOTE_TS,
    };
    assert_eq!(session.read(&format!("[{QUOTE}]")), Ok(Frame::Books(vec![book("AAPL", "87.66", "87.68")])));
    // Quotes may share an array.
    let msft = QUOTE.replace(r#""S":"AAPL""#, r#""S":"MSFT""#).replace("87.66", "411.02").replace("87.68", "411.05");
    let both = format!("[{QUOTE},{msft}]");
    let expected = vec![book("AAPL", "87.66", "87.68"), book("MSFT", "411.02", "411.05")];
    assert_eq!(session.read(&both), Ok(Frame::Books(expected)));
    assert_eq!(session.read("[]"), Ok(Frame::Books(Vec::new())));
}

#[test]
fn a_side_with_no_price_and_no_size_gives_no_book() {
    let mut session = session();
    for one_sided in [
        QUOTE.replace(r#""bp":87.66,"bs":1"#, r#""bp":0,"bs":0"#),
        QUOTE.replace(r#""ap":87.68,"as":4"#, r#""ap":0,"as":0"#),
    ] {
        assert_eq!(session.read(&format!("[{one_sided}]")), Ok(Frame::Books(Vec::new())), "{one_sided}");
    }
}

#[test]
fn an_error_that_ends_the_session_ends_the_connection() {
    for (code, meaning) in [
        (401, "not authenticated"),
        (402, "auth failed"),
        (403, "already authenticated"),
        (404, "auth timeout"),
        (406, "connection limit exceeded"),
        (409, "insufficient subscription"),
    ] {
        let error = format!(r#"[{{"T":"error","code":{code},"msg":"{meaning}"}}]"#);
        let expected = format!("error {code}: {meaning}");
        assert_eq!(session().read(&error), Err(Refusal::Connection(expected)), "{error}");
    }
}

#[test]
fn a_refused_subscription_and_other_errors_leave_the_connection_up() {
    for (code, meaning) in [
        (400, "invalid syntax"),
        (405, "symbol limit exceeded"),
        (407, "slow client"),
        (410, "invalid subscribe action for this feed"),
        (500, "internal error"),
    ] {
        let error = format!(r#"[{{"T":"error","code":{code},"msg":"{meaning}"}}]"#);
        let expected = format!("error {code}: {meaning}");
        assert_eq!(session().read(&error), Err(Refusal::Message(expected)), "{error}");
    }
    let undocumented = r#"[{"T":"error","code":408,"msg":"?"}]"#;
    let expected = "error 408, a code Alpaca does not document".to_owned();
    assert_eq!(session().read(undocumented), Err(Refusal::Message(expected)));
    let without = r#"[{"T":"error","msg":"?"}]"#;
    assert_eq!(session().read(without), Err(Refusal::Message("an error without a code".into())));
    // A confirmation that leaves a symbol out is a refused subscription too.
    let partial = SUBSCRIBED.replace(r#""quotes":["AAPL","MSFT"]"#, r#""quotes":["AAPL"]"#);
    let expected = "the subscription confirms quotes for 1 of 2 symbols".to_owned();
    assert_eq!(session().read(&partial), Err(Refusal::Message(expected)));
    let none = SUBSCRIBED.replace(r#""quotes":["AAPL","MSFT"],"#, "");
    assert!(matches!(session().read(&none), Err(Refusal::Message(reason)) if reason.contains("0 of 2")));
}

#[test]
fn malformed_messages_and_quotes_are_refused() {
    let quote = |edit: &dyn Fn(&str) -> String| format!("[{}]", edit(QUOTE));
    let cases = [
        quote(&|q| q.replace("87.68", "8.768e1")),
        quote(&|q| q.replace(r#""bs":1"#, r#""bs":0"#)),
        quote(&|q| q.replace(r#""as":4"#, r#""as":-4"#)),
        quote(&|q| q.replace(r#""bp":87.66"#, r#""bp":"87.66""#)),
        quote(&|q| q.replace(r#""bp":87.66,"#, "")),
        quote(&|q| q.replace(r#""as":4,"#, "")),
        quote(&|q| q.replace(r#","t":"2021-02-22T15:51:45.335689322Z""#, "")),
        quote(&|q| q.replace("2021-02-22T15:51:45.335689322Z", "1614009105335")),
        quote(&|q| q.replace(r#""S":"AAPL","#, "")),
        quote(&|q| q.replace(r#""S":"AAPL""#, r#""S":"NVDA""#)),
        quote(&|q| q.replace(r#""T":"q""#, r#""T":"t""#)),
        quote(&|q| q.replace(r#""T":"q""#, r#""T":"Q""#)),
        format!("[{QUOTE},{}]", &CONNECTED[1..CONNECTED.len() - 1]),
        r#"[{"T":"success","msg":"subscribed"}]"#.to_owned(),
        QUOTE.to_owned(),
        r#"[{"msg":"connected"}]"#.to_owned(),
        "[".to_owned(),
    ];
    for case in cases {
        assert!(matches!(session().read(&case), Err(Refusal::Message(_))), "accepted {case}");
    }
}

#[test]
fn no_refusal_names_text_alpaca_sent() {
    let echo = "VENDOR-TEXT";
    let cases = [
        AUTH_FAILED.replace("auth failed", echo),
        r#"[{"T":"error","code":408,"msg":"auth failed"}]"#.replace("auth failed", echo),
        AUTHENTICATED.replace("authenticated", echo),
        SUBSCRIBED.replace(r#""quotes":["AAPL","MSFT"]"#, &format!(r#""quotes":["{echo}"]"#)),
        format!(r#"[{{"T":"{echo}"}}]"#),
        format!(r#"[{{"T":"{echo}"}},"#),
        format!("[{}]", QUOTE.replace(r#""S":"AAPL""#, &format!(r#""S":"{echo}""#))),
        format!("[{}]", QUOTE.replace("2021-02-22T15:51:45.335689322Z", echo)),
        format!("[{}]", QUOTE.replace("87.66", &format!(r#""{echo}""#))),
        format!("[{}]", QUOTE.replace(r#""bs":1"#, &format!(r#""bs":"{echo}""#))),
        format!("[{}]", QUOTE.replace(r#""S":"AAPL""#, &format!(r#""S":"AAPL {echo}""#))),
    ];
    for message in cases {
        match session().read(&message) {
            Err(Refusal::Message(reason) | Refusal::Connection(reason)) => {
                assert!(!reason.contains(echo), "{reason} for {message}")
            }
            accepted => panic!("{message} accepted: {accepted:?}"),
        }
    }
}

// ── Helpers ──

/// A session for AAPL and MSFT.
fn session() -> Session {
    Session::new(KEY_ID, SECRET_KEY, &["MSFT", "AAPL"])
}
