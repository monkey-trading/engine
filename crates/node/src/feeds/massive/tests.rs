use super::*;

// The event shapes from Massive's WebSocket documentation for each feed.
const FOREX: &str = r#"[{"ev":"C","p":"EUR/USD","x":44,"a":1.16272,"b":1.1627,"t":1792489875714}]"#;
const FUTURES: &str = r#"[{"ev":"Q","sym":"ESZ6","bp":6512.25,"bs":100,"bt":1792489875700,"ap":6512.5,"as":160,"at":1792489875710,"t":1792489875714}]"#;
const STOCKS: &str = r#"[{"ev":"Q","sym":"AAPL","bx":4,"bp":231.12,"bs":100,"ax":7,"ap":231.14,"as":160,"c":0,"i":[604],"t":1792489875714,"pt":1792489875713,"q":50385480,"z":3}]"#;
const CONNECTED: &str = r#"[{"ev":"status","status":"connected","message":"Connected Successfully"}]"#;
const AUTHENTICATED: &str = r#"[{"ev":"status","status":"auth_success","message":"authenticated"}]"#;
const SUBSCRIBED: &str = r#"[{"ev":"status","status":"success","message":"subscribed to: C.EUR-USD"}]"#;

#[test]
fn a_connection_authenticates_then_subscribes_on_its_feed_channel() {
    let mut forex = Session::new("wss://socket.massive.com/forex", "ms-key", &["EUR-USD", "GBP-USD"]);
    let auth: serde_json::Value = serde_json::from_str(&forex.opening()[0]).unwrap();
    assert_eq!(auth, json!({ "action": "auth", "params": "ms-key" }));
    assert_eq!(forex.read(CONNECTED), Ok(Frame::Books(vec![])));
    let subscription = json!({ "action": "subscribe", "params": "C.EUR-USD,C.GBP-USD" }).to_string();
    assert_eq!(forex.read(AUTHENTICATED), Ok(Frame::Reply(vec![subscription])));
    assert_eq!(forex.read(SUBSCRIBED), Ok(Frame::Books(vec![])));

    // The channel follows the URL's path, whatever follows it.
    let mut forex = Session::new("wss://socket.massive.com/forex?cluster=1", "ms-key", &["EUR-USD"]);
    let subscription = json!({ "action": "subscribe", "params": "C.EUR-USD" }).to_string();
    assert_eq!(forex.read(AUTHENTICATED), Ok(Frame::Reply(vec![subscription])));

    let mut futures = Session::new("wss://socket.massive.com/futures", "ms-key", &["ESZ6", "ESH7"]);
    let subscription = json!({ "action": "subscribe", "params": "Q.ESZ6,Q.ESH7" }).to_string();
    assert_eq!(futures.read(AUTHENTICATED), Ok(Frame::Reply(vec![subscription])));
}

#[test]
fn quotes_give_books_at_massive_time_with_fx_pairs_named_as_subscribed() {
    let book = |symbol: &str, bid: &str, ask: &str| Book {
        symbol: symbol.into(),
        bid: bid.parse().unwrap(),
        ask: ask.parse().unwrap(),
        ts: 1_792_489_875_714,
    };
    let mut forex = Session::new("wss://socket.massive.com/forex", "ms-key", &["EUR-USD"]);
    assert_eq!(forex.read(FOREX), Ok(Frame::Books(vec![book("EUR-USD", "1.1627", "1.16272")])));
    let mut futures = Session::new("wss://socket.massive.com/futures", "ms-key", &["ESZ6"]);
    assert_eq!(futures.read(FUTURES), Ok(Frame::Books(vec![book("ESZ6", "6512.25", "6512.5")])));
    let mut stocks = Session::new("wss://socket.massive.com/stocks", "ms-key", &["AAPL"]);
    assert_eq!(stocks.read(STOCKS), Ok(Frame::Books(vec![book("AAPL", "231.12", "231.14")])));
}

#[test]
fn a_quote_missing_a_side_gives_no_book() {
    let mut stocks = Session::new("wss://socket.massive.com/stocks", "ms-key", &["AAPL"]);
    let one_sided = STOCKS.replace(r#""ap":231.14,"#, "");
    assert_eq!(stocks.read(&one_sided), Ok(Frame::Books(vec![])));
}

#[test]
fn a_failed_authentication_ends_the_connection() {
    let mut forex = Session::new("wss://socket.massive.com/forex", "ms-key", &["EUR-USD"]);
    let failed = r#"[{"ev":"status","status":"auth_failed","message":"authentication failed"}]"#;
    assert!(matches!(forex.read(failed), Err(Refusal::Connection(reason)) if reason.contains("authentication failed")));
}

#[test]
fn malformed_messages_are_refused() {
    let cases = [
        FOREX.replace("1.16272", "1.16272e0"),
        FOREX.replace(r#","t":1792489875714"#, ""),
        FOREX.replace(r#""p":"EUR/USD","#, ""),
        FUTURES.replace(r#""bs":100"#, r#""bs":0"#),
        FUTURES.replace(r#""ev":"Q""#, r#""ev":"T""#),
        r#"[{"ev":"status","status":"max_connections","message":"Maximum number of connections exceeded."}]"#
            .to_owned(),
        r#"{"ev":"C"}"#.to_owned(),
    ];
    for case in cases {
        let mut session = Session::new("wss://socket.massive.com/futures", "ms-key", &["ESZ6"]);
        assert!(matches!(session.read(&case), Err(Refusal::Message(_))), "accepted {case}");
    }
}
