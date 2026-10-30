//! The provider backfill: the venue's candles wire shape, window paging, source mapping, and the
//! endpoint's request against a loopback stand-in.

use std::collections::BTreeMap;

use engine_core::Fixed;

use super::{Candle, Candles, Source, parse_candles, sources, windows};
use crate::config::{FeedProvider, FeedsSection, Venue, VenueConnection, VenueSymbol};

/// The venue's row order: time, low, high, open, close, volume.
const ROWS: &str = r#"[
    [1792489800, "2370.1", "2380.5", "2375.2", "2378.9", "12.5"],
    [1792489740, 2368.2, 2376.4, 2371.0, 2375.2, 10.1]
]"#;

#[test]
fn the_wire_rows_parse_newest_first() {
    let candles = parse_candles(ROWS).unwrap();
    assert_eq!(
        candles,
        [
            Candle {
                ts: 1_792_489_800_000,
                open: fixed("2375.2"),
                high: fixed("2380.5"),
                low: fixed("2370.1"),
                close: fixed("2378.9")
            },
            Candle {
                ts: 1_792_489_740_000,
                open: fixed("2371.0"),
                high: fixed("2376.4"),
                low: fixed("2368.2"),
                close: fixed("2375.2")
            },
        ]
    );
}

#[test]
fn malformed_rows_are_refused_not_guessed() {
    let cases = [
        r#"[["not-a-time", "1", "1", "1", "1", "1"]]"#,
        r#"[[-1, "1", "1", "1", "1", "1"]]"#,
        r#"[[1, "1", "1", "1", "1"]]"#,
        r#"[[1, "1", "1", "1", "1", "1", "1"]]"#,
        r#"[[1, true, "1", "1", "1", "1"]]"#,
        r#"[[1, "nan-ish", "1", "1", "1", "1"]]"#,
        r#"[[1, "0.0000000001", "1", "1", "1", "1"]]"#,
        r#"[[1, "100000000000000000000000000000000000000000", "1", "1", "1", "1"]]"#,
        r#"{"channel":"candles"}"#,
        r#"[1, 2, 3]"#,
        "not json",
    ];
    for case in cases {
        assert!(parse_candles(case).is_err(), "parsed {case}");
    }
}

#[test]
fn windows_page_backwards_without_overlap() {
    let now = 1_792_489_800_000;
    // A depth of ten minutes at one-minute granularity: 300 candles a page, so one page covers
    // it, ending at now and starting at the oldest bucket.
    let one_page = windows(now, 10 * 60_000, 60_000);
    assert_eq!(one_page, [(now - 10 * 60_000, now)], "the page starts at the oldest bucket");
    // Five hundred buckets needs two pages: the second ends one bucket before the first's oldest,
    // and starts at the depth's oldest bucket, which clamps it.
    let two_pages = windows(now, 500 * 60_000, 60_000);
    assert_eq!(two_pages.len(), 2);
    assert_eq!(two_pages[0], (now - 299 * 60_000, now));
    assert_eq!(two_pages[1], (now - 500 * 60_000, now - 300 * 60_000));
    // The pages step backwards without overlap: the second page's end sits one bucket before the
    // first page's start, and no page starts before the depth's oldest bucket.
    assert_eq!(two_pages[1].1, two_pages[0].0 - 60_000);
    assert!(two_pages.iter().all(|&(start, _)| start >= now - 500 * 60_000));
}

#[test]
fn only_coinbase_connections_backfill() {
    let mapped = sources(&venue_feeds());
    assert_eq!(
        mapped,
        BTreeMap::from([
            (1, Source { venue: Venue::Coinbase, symbol: "BTC-USD".to_owned() }),
            (2, Source { venue: Venue::Coinbase, symbol: "ETH-USD".to_owned() }),
        ]),
        "the venue without a candles adapter maps nothing"
    );
    let mut mock_only = venue_feeds();
    mock_only.venues = None;
    assert!(sources(&mock_only).is_empty());
}

/// Feeds over two venues, one with a candles adapter and one without.
fn venue_feeds() -> FeedsSection {
    FeedsSection {
        provider: FeedProvider::Venues,
        interval: std::time::Duration::from_millis(250),
        mock_mids: Vec::new(),
        venues: Some(crate::config::VenuesSection {
            timeout: std::time::Duration::from_secs(10),
            reconnect_min: std::time::Duration::from_millis(500),
            reconnect_max: std::time::Duration::from_secs(30),
            stale_after: std::time::Duration::from_secs(1),
            connections: vec![
                VenueConnection {
                    venue: Venue::Coinbase,
                    url: "wss://advanced-trade-ws.coinbase.com".to_owned(),
                    api_key_env: None,
                    dataset: None,
                    symbols: vec![
                        VenueSymbol { market: 1, symbol: "BTC-USD".to_owned(), contract: None },
                        VenueSymbol { market: 2, symbol: "ETH-USD".to_owned(), contract: None },
                    ],
                },
                VenueConnection {
                    venue: Venue::Kraken,
                    url: "wss://ws.kraken.com/v2".to_owned(),
                    api_key_env: None,
                    dataset: None,
                    symbols: vec![VenueSymbol { market: 1, symbol: "BTC/USD".to_owned(), contract: None }],
                },
            ],
        }),
    }
}

/// A candles request against a loopback stand-in returns its parsed rows.
#[tokio::test]
async fn fetch_asks_the_stand_in_and_parses_its_rows() {
    let app = axum::Router::new().route(
        "/products/BTC-USD/candles",
        axum::routing::get(|query: axum::extract::Query<BTreeMap<String, String>>| async move {
            assert_eq!(query.get("granularity").map(String::as_str), Some("60"));
            axum::Json(serde_json::json!([[1792489800, "2370.1", "2380.5", "2375.2", "2378.9", "12.5"]]))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let candles = Candles::new(&format!("http://{addr}")).unwrap();
    let rows = candles.fetch("BTC-USD", 60, 1_792_489_740_000, 1_792_489_800_000).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].close, fixed("2378.9"));
}

fn fixed(text: &str) -> Fixed {
    text.parse().unwrap()
}
