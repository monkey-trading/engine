use super::*;

use serde_json::Value;

/// CME's login status, as its message specification spells it.
const LOGIN: &str = r#"{"header":{"messageType":"LOGIN_STATUS","sentTime":"2023-11-21T16:47:52.155000000Z","version":"1.0","sequenceNumber":"0"},"payload":{"status":"AUTHENTICATED"}}"#;
/// The subscription answer of CME's message examples, for the products this test subscribes to.
const SUBSCRIBED: &str = r#"{"header":{"messageType":"SUBSCRIPTION_STATUS","requestId":"1","sentTime":"2023-11-28T15:46:20.532000000Z","version":"1.0","sequenceNumber":"2"},"payload":{"status":"SUBSCRIBED","subscriptionMessageTypes":["TOB"],"subscriptions":[{"productType":"FUT","productCode":"ES"}]}}"#;
/// A refused subscription, verbatim from CME's message examples.
const REFUSED: &str = r#"{"header":{"messageType":"SUBSCRIPTION_ERROR","version":"1.0","requestId":"1","token":"","sequenceNumber":"1"},"payload":{"subscriptionMessageTypes":["TRD","TOB"],"subscriptions":[{"productType":"FUT&&&","productCode":"6A"},{"productType":"FUT","productCode":"6J"}]},"errors":[{"code":"ERROR_400","message":"Invalid productType field","referenceIndex":0}]}"#;

#[test]
fn an_accepted_login_is_answered_with_the_subscription_of_each_product_whole() {
    let mut session = Session::new(&["NQ:202612", "ES:202703", "ES:202612"]);
    let Ok(Frame::Reply(reply)) = session.read(LOGIN) else { panic!("no subscription") };
    let subscription: Value = serde_json::from_str(&reply[0]).unwrap();
    // No period is named, so one CME no longer lists cannot refuse the product's others.
    let entry =
        |product: &str| json!({ "productType": "FUT", "productCode": product, "spreadReportTypes": ["OUTRIGHT"] });
    let expected = json!({
        "header": { "messageType": "SUBSCRIBE", "version": "1.0" },
        "payload": { "subscriptionMessageTypes": ["TOB"], "subscriptions": [entry("ES"), entry("NQ")] },
    });
    assert_eq!(subscription, expected);
    assert_eq!(session.read(SUBSCRIBED), Ok(Frame::Subscribed));
    // The spelling of CME's message examples is a login too.
    let example = r#"{"header":{"messageType":"LOGINSTATUS","sentTime":"2023-11-21T16:47:52.155000000Z","version":"1.0","sequenceNumber":"0"},"payload":{"status":"authenticated"}}"#;
    assert!(matches!(session.read(example), Ok(Frame::Reply(_))));
    // As is a status written as an array, as the login specification's field table writes it.
    let array = LOGIN.replace(r#""payload":{"status":"AUTHENTICATED"}"#, r#""payload":[{"status":"AUTHENTICATED"}]"#);
    assert!(matches!(session.read(&array), Ok(Frame::Reply(_))));
}

#[test]
fn a_top_of_book_restates_each_instrument_quoting_both_sides_at_its_sent_time() {
    let mut session = Session::new(&["ES:202612", "ES:202703", "ES:202706"]);
    let message = top_of_book(&[
        top("ES", "202612", &[("6000.25", "12")], &[("6000.5", "7")]),
        top("ES", "202703", &[("6040", "3")], &[("6040.25", "1")]),
        // No offers yet: nothing to quote.
        top("ES", "202706", &[("6080", "1")], &[]),
        // A contract of the product that no symbol names is passed over, even one that would be
        // refused.
        top("ES", "202709", &[("6120", "0")], &[("6119", "1"), ("6121", "1")]),
    ]);
    // 2026-11-13T15:04:05.123456789Z, to the millisecond.
    let ts = 1_794_582_245_123;
    let book = |symbol: &str, bid: &str, ask: &str| Book {
        symbol: symbol.into(),
        bid: bid.parse().unwrap(),
        ask: ask.parse().unwrap(),
        ts,
    };
    let expected = vec![book("ES:202612", "6000.25", "6000.5"), book("ES:202703", "6040", "6040.25")];
    assert_eq!(session.read(&message), Ok(Frame::Books(expected)));
    // An instrument a symbol names with neither side gives no book.
    let neither = top_of_book(&[top("ES", "202706", &[], &[])]);
    assert_eq!(session.read(&neither), Ok(Frame::Books(Vec::new())));
    // CME's message example of an empty book, of a product no symbol names, is passed over.
    let empty = r#"{"header":{"messageType":"TOB","sentTime":"2023-11-28T16:06:36.948000000Z","sequenceNumber":"728","version":"1.0"},"payload":[{"instrument":{"definitionSource":"E","exchangeMic":"XNYM","id":"735995","marketSeqmentId":"80","periodCode":"","productCode":"CL","productGroup":"CL","productType":"FUT","symbol":"CLG34"},"askLevel":[],"bidLevel":[]}]}"#;
    assert_eq!(session.read(empty), Ok(Frame::Books(Vec::new())));
}

/// CME's message example of an empty book (Example 7), which sends no `periodCode`.
const EXAMPLE_7: &str = r#"{"header":{"messageType":"TOB","sentTime":"2023-11-28T16:06:36.948000000Z","sequenceNumber":"728","version":"1.0"},"payload":[{"instrument":{"definitionSource":"E","exchangeMic":"XNYM","id":"735995","marketSeqmentId":"80","periodCode":"","productCode":"CL","productGroup":"CL","productType":"FUT","symbol":"CLG34"},"askLevel":[],"bidLevel":[]}]}"#;

#[test]
fn a_future_of_a_subscribed_product_with_no_yyyymm_period_is_refused_not_passed_over() {
    // Under a session naming a CL contract, Example 7's instrument is a CL future CME sent without
    // its period, and the refusal reaches the alert (#723).
    let mut session = Session::new(&["CL:203402", "ES:202612"]);
    assert_eq!(
        session.read(EXAMPLE_7),
        Err(Refusal::Message("a future of CL with a periodCode that is not YYYYMM".into()))
    );
    // One malformed future refuses the message, though another instrument in it is quoted.
    let mixed = top_of_book(&[top("ES", "202612", &[("6000", "1")], &[("6001", "1")]), top_with_period("")]);
    assert!(matches!(session.read(&mixed), Err(Refusal::Message(_))));
    for period in ["26", "2026-12", "202613", "202600", "abcdef", "2026123"] {
        let message = top_of_book(&[top_with_period(period)]);
        assert!(matches!(session.read(&message), Err(Refusal::Message(_))), "{period:?}");
    }
    // The refusal names no text of CME's: a hostile period never reaches it.
    let hostile = top_of_book(&[top_with_period("token-123")]);
    assert_eq!(
        session.read(&hostile),
        Err(Refusal::Message("a future of ES with a periodCode that is not YYYYMM".into()))
    );
    // A product no symbol names, and an instrument that is no future, are still passed over.
    let other = top_of_book(&[top_with_period("").replace(r#""productCode":"ES""#, r#""productCode":"NQ""#)]);
    assert_eq!(session.read(&other), Ok(Frame::Books(Vec::new())));
    let option = top_of_book(&[top_with_period("").replace(r#""productType":"FUT""#, r#""productType":"OOF""#)]);
    assert_eq!(session.read(&option), Ok(Frame::Books(Vec::new())));
}

#[test]
fn an_empty_book_of_a_named_contract_is_example_7s_shape_and_quotes_nothing() {
    let mut session = Session::new(&["CL:203402"]);
    let named = EXAMPLE_7.replace(r#""periodCode":"""#, r#""periodCode":"203402""#);
    assert_eq!(session.read(&named), Ok(Frame::Books(Vec::new())));
}

#[test]
fn heartbeats_are_control_and_a_refused_subscription_is_refused_with_its_errors() {
    let mut session = Session::new(&["ES:202612"]);
    let heartbeat = r#"{"header":{"messageType":"HEARTBEAT","sentTime":"2026-11-13T15:04:05.123456789Z","sequenceNumber":"3","version":"1.0"}}"#;
    assert_eq!(session.read(heartbeat), Ok(Frame::Control));
    let refused = session.read(REFUSED);
    assert_eq!(refused, Err(Refusal::Message("subscription refused: ERROR_400".into())));
}

#[test]
fn a_refused_or_ended_login_or_subscription_ends_the_connection() {
    // CME sends a login status whenever it ends a session, as at a token's expiry; only an
    // authenticated one is a login.
    let ended = LOGIN.replace("AUTHENTICATED", "EXPIRED");
    let cases = [
        (
            r#"{"header":{"messageType":"LOGINERR","sentTime":"2026-11-13T15:04:05Z","version":"1.0"}}"#.to_owned(),
            "login refused",
        ),
        (ended, "login status not authenticated"),
        (LOGIN.replace(r#","payload":{"status":"AUTHENTICATED"}"#, ""), "login status without one status"),
        (SUBSCRIBED.replace(r#""SUBSCRIBED""#, r#""UNSUBSCRIBED""#), "subscription status not subscribed"),
        // A status array of more than one, or of none: CME documents no precedence.
        (
            SUBSCRIBED.replace(
                r#""payload":{"status":"SUBSCRIBED","#,
                r#""payload":[{"status":"SUBSCRIBED"},{"status":"UNSUBSCRIBED"}],"ignored":{"#,
            ),
            "subscription status without one status",
        ),
        (LOGIN.replace(r#"{"status":"AUTHENTICATED"}"#, "[]"), "login status without one status"),
    ];
    for (message, reason) in cases {
        let mut session = Session::new(&["ES:202612"]);
        assert_eq!(session.read(&message), Err(Refusal::Connection(reason.into())), "{message}");
    }
}

#[test]
fn malformed_messages_and_tops_of_book_are_refused() {
    let good = top_of_book(&[top("ES", "202612", &[("6000.25", "12")], &[("6000.5", "7")])]);
    let cases = [
        // A price or size that is not a decimal, or a size that is not positive.
        good.replace(r#""price":"6000.25""#, r#""price":"6000,25""#),
        good.replace(r#""price":"6000.5""#, r#""price":"6e3""#),
        good.replace(r#""qty":"12""#, r#""qty":"0""#),
        good.replace(r#""qty":"7""#, r#""qty":"-7""#),
        good.replace(r#""qty":"7""#, r#""qty":"1000000001""#),
        // A time that is missing or not RFC 3339.
        good.replace(r#""sentTime":"2026-11-13T15:04:05.123456789Z","#, ""),
        good.replace("2026-11-13T15:04:05.123456789Z", "20261113-15:04:05.123"),
        // A spread, as a multi-leg instrument or as a future whose period is its first leg's, or one
        // contract named twice; a side deeper than one level, or an instrument without its period.
        good.replace(r#""productType":"FUT""#, r#""productType":"MLEG""#),
        good.replace(r#""symbol":"ESZ6""#, r#""symbol":"ESZ6-ESH7""#),
        good.replace(r#""symbol":"ESZ6""#, r#""symbol":"ES""#),
        top_of_book(&[
            top("ES", "202612", &[("6000.25", "12")], &[("6000.5", "7")]),
            top("ES", "202612", &[("6000", "1")], &[("6000.75", "1")]),
        ]),
        good.replace(
            r#"[{"lastUpdateTime""#,
            r#"[{"lastUpdateTime":"2026-11-13T15:04:05Z","orderCnt":"1","price":"6000","qty":"1"},{"lastUpdateTime""#,
        ),
        good.replace(r#""periodCode":"202612","#, ""),
        // An unknown type, or not a message at all.
        good.replace(r#""messageType":"TOB""#, r#""messageType":"TRD""#),
        "not json".to_owned(),
        "[]".to_owned(),
    ];
    for case in cases {
        let mut session = Session::new(&["ES:202612"]);
        assert!(matches!(session.read(&case), Err(Refusal::Message(_))), "accepted {case}");
    }
    assert!(matches!(Session::new(&["ES:202612"]).read(&good), Ok(Frame::Books(books)) if books.len() == 1));
    // A contract a decade out has a two-digit year, as CME's message example's `CLG34` does.
    let decade = good.replace(r#""symbol":"ESZ6""#, r#""symbol":"ESZ26""#);
    assert!(matches!(Session::new(&["ES:202612"]).read(&decade), Ok(Frame::Books(books)) if books.len() == 1));
}

#[test]
fn no_refusal_names_text_cme_sent() {
    // Each free-text field CME could fill, carrying text that must not reach a log line.
    let echo = "VENDOR-TEXT";
    let good = top_of_book(&[top("ES", "202612", &[("6000.25", "12")], &[("6000.5", "7")])]);
    let messages = [
        LOGIN.replace("AUTHENTICATED", echo),
        SUBSCRIBED.replace(r#""status":"SUBSCRIBED""#, &format!(r#""status":"{echo}""#)),
        REFUSED.replace("Invalid productType field", echo),
        REFUSED.replace("ERROR_400", echo),
        LOGIN.replace("LOGIN_STATUS", echo),
        format!(r#"{{"header":{{"messageType":"{echo}"}}}}"#),
        format!(r#"{{"header":{{"messageType":"{echo}"}},"#),
        good.replace(r#""payload":["#, &format!(r#""payload":"{echo}","ignored":["#)),
        good.replace(r#""productType":"FUT""#, &format!(r#""productType":"{echo}""#)),
        good.replace(r#""symbol":"ESZ6""#, &format!(r#""symbol":"{echo}""#)),
        good.replace("2026-11-13T15:04:05.123456789Z", echo),
        good.replace(r#""price":"6000.25""#, &format!(r#""price":"{echo}""#)),
        good.replace(r#""qty":"12""#, &format!(r#""qty":"{echo}""#)),
    ];
    for message in messages {
        match Session::new(&["ES:202612"]).read(&message) {
            Err(Refusal::Message(reason) | Refusal::Connection(reason)) => {
                assert!(!reason.contains(echo), "{reason} for {message}")
            }
            read => panic!("{read:?} for {message}"),
        }
    }
}

#[test]
fn a_symbol_must_name_the_contract_it_quotes() {
    for (symbol, contract) in [
        ("ES:202612", "ESZ6"),
        ("NQ:202703", "NQH7"),
        ("NIY:202709", "NIYU7"),
        ("GC:202702", "GCG7"),
        ("SI:202805", "SIK8"),
        ("CL:202801", "CLF8"),
    ] {
        assert_eq!(check_symbol(symbol, Some(contract)), Ok(()), "{symbol}");
    }
    let refused = [
        ("ESZ6", Some("ESZ6"), "not <product>:<YYYYMM>"),
        ("es:202612", Some("ESZ6"), "product code"),
        (":202612", Some("ESZ6"), "product code"),
        ("ES:20261", Some("ESZ6"), "is not YYYYMM"),
        ("ES:202613", Some("ESZ6"), "is not YYYYMM"),
        ("ES:202600", Some("ESZ6"), "is not YYYYMM"),
        ("ES:2026-1", Some("ESZ6"), "is not YYYYMM"),
        ("ES:202612", None, "names the contract it quotes"),
        ("ES:202603", Some("ESZ6"), "names ESH6, not contract ESZ6"),
        ("ES:202712", Some("ESZ6"), "names ESZ7, not contract ESZ6"),
        ("NQ:202612", Some("ESZ6"), "names NQZ6, not contract ESZ6"),
    ];
    for (symbol, contract, reason) in refused {
        let err = check_symbol(symbol, contract).unwrap_err();
        assert!(err.contains(reason), "{symbol}: expected {reason:?}, got {err}");
    }
}

// ── Helpers ──

/// A `TOB` message carrying `tops`, sent at 2026-11-13T15:04:05.123456789Z.
fn top_of_book(tops: &[String]) -> String {
    format!(
        r#"{{"header":{{"messageType":"TOB","sentTime":"2026-11-13T15:04:05.123456789Z","sequenceNumber":"728","version":"1.0"}},"payload":[{}]}}"#,
        tops.join(",")
    )
}

/// One instrument's top of book in CME's `TOB` field layout, each level a price and quantity.
fn top(product: &str, period: &str, bids: &[(&str, &str)], asks: &[(&str, &str)]) -> String {
    let levels = |levels: &[(&str, &str)]| {
        let levels: Vec<String> = levels
            .iter()
            .map(|(price, qty)| {
                format!(r#"{{"lastUpdateTime":"2026-11-13T15:04:04.000000000Z","orderCnt":"2","price":"{price}","qty":"{qty}"}}"#)
            })
            .collect();
        format!("[{}]", levels.join(","))
    };
    format!(
        r#"{{"instrument":{{"definitionSource":"E","exchangeMic":"XCME","id":"118","marketSegmentId":"72","periodCode":"{period}","productCode":"{product}","productGroup":"{product}","productType":"FUT","symbol":"{}"}},"askLevel":{},"bidLevel":{}}}"#,
        globex(product, period, 1),
        levels(asks),
        levels(bids)
    )
}

/// An ES future's top of book with `period` as its `periodCode`, whatever it is.
fn top_with_period(period: &str) -> String {
    top("ES", "202612", &[], &[]).replace(r#""periodCode":"202612""#, &format!(r#""periodCode":"{period}""#))
}
