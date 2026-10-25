use super::*;

const KEY: &str = "db-key-0123456789abcdef0123456789ab";
// Records in the shape of Databento's JSON encoding: 64-bit integers as strings.
const MAPPING: &str = r#"{"hd":{"ts_event":"1792489875714000000","rtype":22,"publisher_id":1,"instrument_id":42140878},"stype_in":255,"stype_in_symbol":"ESZ6","stype_out":0,"stype_out_symbol":"ESZ6","start_ts":"1792454400000000000","end_ts":"1792540800000000000"}"#;
const TOP: &str = r#"{"ts_recv":"1792489875714712345","hd":{"ts_event":"1792489875714123456","rtype":1,"publisher_id":1,"instrument_id":42140878},"action":"A","side":"B","depth":0,"price":"6512250000000","size":3,"flags":128,"ts_in_delta":13000,"sequence":11,"levels":[{"bid_px":"6512250000000","ask_px":"6512500000000","bid_sz":12,"ask_sz":7,"bid_ct":4,"ask_ct":3}]}"#;
const HEARTBEAT: &str = r#"{"hd":{"ts_event":"1792489875714000000","rtype":23,"publisher_id":0,"instrument_id":0},"msg":"Heartbeat","code":0}"#;

#[test]
fn the_handshake_answers_the_challenge_then_subscribes_and_starts_the_session() {
    let mut session = Session::new(KEY, "GLBX.MDP3", &["ESZ6", "ESH7"]);
    assert_eq!(session.read("lsg_version=0.6.0"), Ok(Frame::Control));
    // The SHA-256 of "challenge123|<key>", then the key's last five characters.
    let auth = "auth=7ff1afe9dd4e62d9745b8529dab7a7f0c15d9dc92d20c845ac40a8644a6cd366-789ab|dataset=GLBX.MDP3|encoding=json|ts_out=0";
    assert_eq!(session.read("cram=challenge123"), Ok(Frame::Reply(vec![auth.into()])));
    let start = vec!["schema=mbp-1|stype_in=raw_symbol|symbols=ESZ6,ESH7".to_owned(), "start_session".to_owned()];
    assert_eq!(session.read("success=1|session_id=5"), Ok(Frame::Reply(start)));
    assert_eq!(session.read(HEARTBEAT), Ok(Frame::Control));
}

#[test]
fn a_failed_authentication_ends_the_connection() {
    let mut session = Session::new(KEY, "GLBX.MDP3", &["ESZ6"]);
    let failed = session.read("success=0|error=Authentication failed.");
    assert_eq!(failed, Err(Refusal::Connection("authentication failed: Authentication failed.".into())));
}

#[test]
fn a_top_of_book_gives_its_mapped_symbol_at_the_event_time() {
    let mut session = streaming(&["ESZ6"]);
    assert_eq!(session.read(MAPPING), Ok(Frame::Control));
    let book = Book {
        symbol: "ESZ6".into(),
        bid: "6512.25".parse().unwrap(),
        ask: "6512.5".parse().unwrap(),
        ts: 1_792_489_875_714,
    };
    assert_eq!(session.read(TOP), Ok(Frame::Books(vec![book])));
    // An empty side has no price, so no book.
    let one_sided = TOP.replace(r#""ask_px":"6512500000000""#, &format!(r#""ask_px":"{UNDEF_PRICE}""#));
    assert_eq!(session.read(&one_sided), Ok(Frame::Books(vec![])));
}

#[test]
fn a_remapped_symbol_keeps_one_instrument_id() {
    let mut session = streaming(&["ESZ6"]);
    session.read(MAPPING).unwrap();
    session.read(&MAPPING.replace("42140878", "7")).unwrap();
    assert_eq!(session.instruments, BTreeMap::from([(7, "ESZ6".to_owned())]));
    assert!(matches!(session.read(TOP), Err(Refusal::Message(reason)) if reason.contains("no mapping")));
}

#[test]
fn records_that_cannot_be_read_are_refused() {
    let cases = [
        MAPPING.replace(r#""stype_in_symbol":"ESZ6""#, r#""stype_in_symbol":"NQZ6""#),
        TOP.replace(r#""bid_px":"6512250000000""#, r#""bid_px":"6512.25""#),
        TOP.replace(r#""bid_sz":12"#, r#""bid_sz":0"#),
        TOP.replace(r#""ts_event":"1792489875714123456""#, r#""ts_event":"soon""#),
        TOP.replace(r#""rtype":1"#, r#""rtype":2"#),
        TOP.replace(r#","levels":[{"bid_px":"6512250000000","ask_px":"6512500000000","bid_sz":12,"ask_sz":7,"bid_ct":4,"ask_ct":3}]"#, ""),
        r#"{"hd":{"ts_event":"0","rtype":21,"publisher_id":0,"instrument_id":0},"err":"Unknown symbol","code":1}"#.to_owned(),
        r#"{"msg":"no header"}"#.to_owned(),
        "not json".to_owned(),
    ];
    for case in cases {
        let mut session = streaming(&["ESZ6"]);
        session.read(MAPPING).unwrap();
        assert!(matches!(session.read(&case), Err(Refusal::Message(_))), "accepted {case}");
    }
}

#[test]
fn metadata_ahead_of_the_records_carries_no_price() {
    let mut session = streaming(&["ESZ6"]);
    assert_eq!(session.read(r#"{"version":3,"dataset":"GLBX.MDP3","schema":"mbp-1"}"#), Ok(Frame::Control));
}

// ── Helpers ──

/// A session past its handshake, streaming records for `symbols`.
fn streaming(symbols: &[&str]) -> Session {
    let mut session = Session::new(KEY, "GLBX.MDP3", symbols);
    session.read("success=1|session_id=5").unwrap();
    session
}
