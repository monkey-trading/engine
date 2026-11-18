//! The kept book against recorded Kraken messages: verified tops, and the resync a failed checksum or
//! a missed update causes.

use super::*;

// Recorded from wss://ws.kraken.com/v2: an ETH/USD snapshot and the seven updates that followed it.
const SNAPSHOT: &str = r#"{"channel":"book","type":"snapshot","data":[{"symbol":"ETH/USD","bids":[{"price":2387.34,"qty":1.11000000},{"price":2387.16,"qty":0.03207130},{"price":2387.15,"qty":2.09886512},{"price":2387.14,"qty":0.01300000},{"price":2387.13,"qty":2.09456314},{"price":2387.06,"qty":0.47001888},{"price":2386.97,"qty":0.44710000},{"price":2386.96,"qty":0.20000000},{"price":2386.89,"qty":0.01600000},{"price":2386.87,"qty":1.54940553}],"asks":[{"price":2387.35,"qty":13.07562979},{"price":2387.42,"qty":1.89091043},{"price":2387.43,"qty":2.09430224},{"price":2387.52,"qty":4.24040608},{"price":2387.53,"qty":16.31597844},{"price":2387.54,"qty":31.41396556},{"price":2387.57,"qty":0.35563916},{"price":2387.59,"qty":0.44710000},{"price":2387.64,"qty":0.13118718},{"price":2387.69,"qty":1.35850000}],"checksum":602132210,"timestamp":"2026-10-20T12:34:44.332552Z"}]}"#;
const UPDATES: [&str; 7] = [
    r#"{"channel":"book","type":"update","data":[{"symbol":"ETH/USD","bids":[],"asks":[{"price":2387.35,"qty":3.07562993}],"checksum":3352854302,"timestamp":"2026-10-20T12:34:44.338822Z"}]}"#,
    r#"{"channel":"book","type":"update","data":[{"symbol":"ETH/USD","bids":[],"asks":[{"price":2387.35,"qty":2.62852993}],"checksum":3241282715,"timestamp":"2026-10-20T12:34:44.389205Z"}]}"#,
    r#"{"channel":"book","type":"update","data":[{"symbol":"ETH/USD","bids":[{"price":2387.34,"qty":1.55710000}],"asks":[],"checksum":589154695,"timestamp":"2026-10-20T12:34:44.390458Z"}]}"#,
    // Removes a bid inside the top ten and adds one at the edge.
    r#"{"channel":"book","type":"update","data":[{"symbol":"ETH/USD","bids":[{"price":2387.15,"qty":0.00000000},{"price":2386.82,"qty":6.37118262}],"asks":[],"checksum":3655021122,"timestamp":"2026-10-20T12:34:44.401224Z"}]}"#,
    r#"{"channel":"book","type":"update","data":[{"symbol":"ETH/USD","bids":[{"price":2387.14,"qty":0.70297503}],"asks":[],"checksum":3968313210,"timestamp":"2026-10-20T12:34:44.401224Z"}]}"#,
    r#"{"channel":"book","type":"update","data":[{"symbol":"ETH/USD","bids":[],"asks":[{"price":2387.35,"qty":0.01675497}],"checksum":2862403204,"timestamp":"2026-10-20T12:34:44.401610Z"}]}"#,
    // Adds an ask inside the top ten, pushing the tenth out: only truncation keeps the checksum.
    r#"{"channel":"book","type":"update","data":[{"symbol":"ETH/USD","bids":[],"asks":[{"price":2387.41,"qty":0.68997503}],"checksum":412518972,"timestamp":"2026-10-20T12:34:44.401610Z"}]}"#,
];
/// The host time messages are read at: just after the recorded ones.
const NOW: Millis = 1_792_499_685_000;
const HEARTBEAT: &str = r#"{"channel":"heartbeat"}"#;
const STATUS: &str = r#"{"channel":"status","type":"update","data":[{"version":"2.0.10","system":"online","api_version":"v2","connection_id":5169314452737955867,"upcoming_maintenance":[],"emergency":[]}]}"#;
const SUBSCRIBED: &str = r#"{"method":"subscribe","result":{"channel":"book","depth":10,"snapshot":true,"symbol":"ETH/USD"},"success":true,"time_in":"2026-10-20T12:34:28.967739Z","time_out":"2026-10-20T12:34:28.967768Z"}"#;
const REJECTED: &str = r#"{"error":"Currency pair not supported BTC/USX","method":"subscribe","success":false,"symbol":"BTC/USX","time_in":"2026-10-20T09:43:52.433866Z","time_out":"2026-10-20T09:43:52.433915Z"}"#;

#[test]
fn a_snapshot_gives_its_verified_top_at_the_venue_time() {
    // 2026-10-20T12:34:44.332552Z, truncated to the millisecond.
    assert_eq!(Books::new(&["ETH/USD"]).read(SNAPSHOT, NOW), Ok(top("2387.34", "2387.35", 1_792_499_684_332)));
}

#[test]
fn every_verified_update_publishes_the_top_at_its_time() {
    let mut books = Books::new(&["ETH/USD"]);
    books.read(SNAPSHOT, NOW).unwrap();
    // The first three change sizes only: the top holds but is published at each update's time.
    let times = [338, 389, 390, 401, 401, 401, 401].map(|ms| 1_792_499_684_000 + ms);
    for (update, ts) in UPDATES.iter().zip(times) {
        assert_eq!(books.read(update, NOW), Ok(top("2387.34", "2387.35", ts)), "{update}");
    }
}

#[test]
fn a_corrupted_checksum_resyncs_without_a_quote() {
    let mut books = Books::new(&["ETH/USD"]);
    books.read(SNAPSHOT, NOW).unwrap();
    let corrupted = UPDATES[0].replace(r#""checksum":3352854302"#, r#""checksum":3352854303"#);
    assert!(
        matches!(books.read(&corrupted, NOW), Err(Refusal::Connection(reason)) if reason.contains("checksum mismatch"))
    );
}

#[test]
fn a_corrupted_level_resyncs_without_a_quote() {
    let mut books = Books::new(&["ETH/USD"]);
    books.read(SNAPSHOT, NOW).unwrap();
    // The price is valid and in sequence, but not the venue's: only the checksum shows it.
    let corrupted = UPDATES[0].replace(r#""qty":3.07562993"#, r#""qty":3.07562994"#);
    assert!(
        matches!(books.read(&corrupted, NOW), Err(Refusal::Connection(reason)) if reason.contains("checksum mismatch"))
    );
}

#[test]
fn a_missed_update_is_caught_by_the_next_checksum() {
    let mut books = Books::new(&["ETH/USD"]);
    books.read(SNAPSHOT, NOW).unwrap();
    books.read(UPDATES[0], NOW).unwrap();
    books.read(UPDATES[1], NOW).unwrap();
    // UPDATES[2] never arrives; the book misses its new bid size.
    assert!(
        matches!(books.read(UPDATES[3], NOW), Err(Refusal::Connection(reason)) if reason.contains("checksum mismatch"))
    );
}

#[test]
fn malformed_books_resync_with_their_reason() {
    let cases = [
        // Exponent notation is not a plain decimal; reading it would mean going through a float.
        ("bad price", UPDATES[0].replace(r#""price":2387.35"#, r#""price":2.38735e3"#)),
        ("bad price", UPDATES[0].replace(r#""price":2387.35"#, r#""price":"2387.35""#)),
        ("out of range", UPDATES[0].replace(r#""qty":3.07562993"#, r#""qty":-3.07562993"#)),
        ("out of range", UPDATES[0].replace(r#""price":2387.35"#, r#""price":0"#)),
        ("bad book", UPDATES[0].replace(r#""checksum":3352854302,"#, "")),
        ("bad timestamp", UPDATES[0].replace("2026-10-20T12:34:44.338822Z", "later")),
        ("before the epoch", UPDATES[0].replace("2026-10-20T12:34:44.338822Z", "1969-12-31T23:59:59.999Z")),
        ("than any clock error", UPDATES[0].replace("2026-10-20T12:34:44.338822Z", "2026-10-22T12:34:44.338822Z")),
        ("unknown book message type", UPDATES[0].replace(r#""type":"update""#, r#""type":"delta""#)),
        ("update before snapshot", UPDATES[0].replace("ETH/USD", "SOL/USD")),
        ("not subscribed", UPDATES[0].replace("ETH/USD", "XRP/USD")),
    ];
    for (expected, case) in cases {
        let mut books = Books::new(&["ETH/USD", "SOL/USD"]);
        books.read(SNAPSHOT, NOW).unwrap();
        let refused = books.read(&case, NOW);
        assert!(
            matches!(&refused, Err(Refusal::Connection(reason)) if reason.contains(expected)),
            "{expected}: {refused:?}"
        );
    }
}

#[test]
fn a_new_snapshot_replaces_the_book() {
    let mut books = Books::new(&["ETH/USD"]);
    books.read(SNAPSHOT, NOW).unwrap();
    UPDATES.iter().for_each(|update| drop(books.read(update, NOW).unwrap()));
    // One level a side: its checksum holds only if nothing of the earlier book is kept.
    let checksum = checksum([("2390.00", "1.00000000"), ("2389.00", "2.00000000")].into_iter());
    let snapshot = format!(
        r#"{{"channel":"book","type":"snapshot","data":[{{"symbol":"ETH/USD","bids":[{{"price":2389.00,"qty":2.00000000}}],"asks":[{{"price":2390.00,"qty":1.00000000}}],"checksum":{checksum},"timestamp":"2026-10-20T12:34:45.000000Z"}}]}}"#
    );
    assert_eq!(books.read(&snapshot, NOW), Ok(top("2389", "2390", 1_792_499_685_000)));
}

#[test]
fn heartbeats_status_and_acknowledgements_carry_no_price() {
    let mut books = Books::new(&["ETH/USD"]);
    assert_eq!(books.read(HEARTBEAT, NOW), Ok(Frame::Control));
    assert_eq!(books.read(STATUS, NOW), Ok(Frame::Control));
    assert_eq!(books.read(SUBSCRIBED, NOW), Ok(Frame::Control));
}

#[test]
fn a_failed_subscription_or_unknown_message_is_refused() {
    let mut books = Books::new(&["ETH/USD"]);
    assert!(
        matches!(books.read(REJECTED, NOW), Err(Refusal::Message(reason)) if reason.contains("Currency pair not supported"))
    );
    assert!(matches!(books.read(r#"{"channel":"trade"}"#, NOW), Err(Refusal::Message(_))));
    assert!(matches!(books.read("{", NOW), Err(Refusal::Message(_))));
}

#[test]
fn the_subscription_asks_for_ten_levels_of_every_symbol() {
    let sent: serde_json::Value = serde_json::from_str(&subscriptions(&["BTC/USD", "ETH/USD"])[0]).unwrap();
    let params = json!({ "channel": "book", "symbol": ["BTC/USD", "ETH/USD"], "depth": 10 });
    assert_eq!(sent, json!({ "method": "subscribe", "params": params }));
}

// ── Helpers ──

/// An ETH/USD top of book.
fn top(bid: &str, ask: &str, ts: i64) -> Frame {
    Frame::Books(vec![Book { symbol: "ETH/USD".into(), bid: bid.parse().unwrap(), ask: ask.parse().unwrap(), ts }])
}
