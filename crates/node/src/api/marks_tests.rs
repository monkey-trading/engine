//! `GET /v1/markets/{id}/marks`: one market's index-mark history, paged under a hard cap, every
//! entry labeled log-derived or seeded-from-provider, signed like every other answer.

use axum::extract::{Path, Query, State};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::marks::{MAX_LIMIT, MarksParams, marks};
use super::tests::serve_state;
use crate::history::marks::store::Store;
use crate::history::marks::{Resolution, Retentions};
use crate::test_support::scratch_dir;

/// A store with one log-derived and one provider-seeded minute for market 1, served by the API.
async fn served_store(name: &str) -> SocketAddr {
    let mut store = Store::open_in(&scratch_dir(name), Retentions::defaults()).unwrap();
    store.observe(&[(1, 1_792_489_860_000 + 10_000, "67000".parse().unwrap())]).unwrap();
    store
        .seed(
            &[crate::history::marks::store::Seed {
                market: 1,
                resolution: Resolution::M1,
                ts: 1_792_489_920_000,
                open: "66000".parse().unwrap(),
                high: "66500".parse().unwrap(),
                low: "65500".parse().unwrap(),
                close: "66200".parse().unwrap(),
            }],
            1_792_489_920_000,
        )
        .unwrap();
    let mut state = crate::api::tests::api_state(true).0;
    state.marks = store.reader();
    serve_state(state).await
}

/// The endpoint's page for `path`, parsed.
async fn page(addr: SocketAddr, path: &str) -> (u16, serde_json::Value) {
    let (status, head, body) = get(addr, path).await;
    let value: serde_json::Value =
        serde_json::from_slice(&body).unwrap_or_else(|_| panic!("JSON body: {}", String::from_utf8_lossy(&body)));
    // Every marks answer is signed like every other read.
    assert!(head.contains("x-engine-signature"), "signed answer, got head: {head}");
    (status, value)
}

#[tokio::test]
async fn a_markets_marks_page_carries_ohlc_provenance_and_the_cap() {
    let addr = served_store("marks-api-page").await;
    let (status, page) = page(addr, "/v1/markets/1/marks?resolution=1m").await;
    assert_eq!(status, 200);
    assert_eq!(page["market"], 1);
    assert_eq!(page["resolution"], "1m");
    assert_eq!(page["max_limit"], MAX_LIMIT);
    assert_eq!(page["next"], serde_json::Value::Null);
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{page}");
    // Oldest first: the log-derived minute, then the provider-seeded one, each labeled.
    assert_eq!(entries[0]["ts"].as_i64().unwrap(), 1_792_489_860_000);
    assert_eq!(entries[0]["open"], "67000");
    assert_eq!(entries[0]["provenance"], "log_derived");
    assert_eq!(entries[1]["ts"].as_i64().unwrap(), 1_792_489_920_000);
    assert_eq!(entries[1]["high"], "66500");
    assert_eq!(entries[1]["provenance"], "seeded_from_provider");
    // Fixed prices serialize as decimal strings so no JSON client loses precision.
    assert!(entries[0]["close"].as_str().unwrap().contains('6'));
}

#[tokio::test]
async fn a_window_wider_than_the_limit_pages_by_next() {
    let mut store = Store::open_in(&scratch_dir("marks-api-pages"), Retentions::defaults()).unwrap();
    let base = 1_792_489_860_000_i64;
    for n in 0_i64..5 {
        store.observe(&[(1, base + n * 60_000, "1".parse().unwrap())]).unwrap();
    }
    let mut state = crate::api::tests::api_state(true).0;
    state.marks = store.reader();
    let addr = serve_state(state).await;

    // Five buckets under a limit of two page three times: `next` is the last entry's time plus
    // one, every bucket appears exactly once, and only the last page is exhausted.
    let mut from = i64::MIN;
    let mut seen = Vec::new();
    for page_no in 0..3 {
        let query = if from == i64::MIN {
            "/v1/markets/1/marks?resolution=1m&limit=2".to_owned()
        } else {
            format!("/v1/markets/1/marks?resolution=1m&from_ms={from}&limit=2")
        };
        let (status, got) = page(addr, &query).await;
        assert_eq!(status, 200);
        let entries = got["entries"].as_array().unwrap();
        assert!(entries.len() <= 2, "{got}");
        seen.extend(entries.iter().map(|entry| entry["ts"].as_i64().unwrap()));
        let exhausted = got["next"].is_null();
        assert_eq!(exhausted, page_no == 2, "{got}: exhaustion only on the last page");
        from = got["next"].as_i64().unwrap_or(i64::MIN);
    }
    assert_eq!(seen, (0..5).map(|n| base + n * 60_000).collect::<Vec<_>>());
    // `to_ms` bounds the window from above.
    let (_, window) =
        page(addr, &format!("/v1/markets/1/marks?resolution=1m&from_ms={base}&to_ms={}", base + 60_000)).await;
    assert_eq!(window["entries"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn caller_mistakes_are_bad_requests_and_unknown_markets_are_empty() {
    let addr = served_store("marks-api-errors").await;
    for (path, why) in [
        ("/v1/markets/abc/marks?resolution=1m", "a malformed market id"),
        ("/v1/markets/1/marks?resolution=5m", "an unknown resolution"),
        ("/v1/markets/1/marks?resolution=1m&limit=0", "a zero limit"),
        ("/v1/markets/1/marks?resolution=1m&limit=2001", "a limit over the cap"),
        ("/v1/markets/1/marks?resolution=1m&from_ms=2&to_ms=1", "an inverted window"),
    ] {
        let (status, body) = page(addr, path).await;
        assert_eq!(status, 400, "{why}: {body}");
    }
    let (status, page) = page(addr, "/v1/markets/99/marks?resolution=1m").await;
    assert_eq!(status, 200);
    assert_eq!(page["entries"].as_array().unwrap().len(), 0, "{page}");
    assert_eq!(page["next"], serde_json::Value::Null);
}

#[tokio::test]
async fn the_handler_answers_without_ever_reaching_the_sequencer() {
    let (state, mut rx) = crate::api::tests::api_state(true);
    let response = marks(
        State(state),
        Path("1".to_owned()),
        Query(MarksParams { resolution: "1m".to_owned(), from_ms: None, to_ms: None, limit: None }),
    );
    assert!(matches!(response.await, Err(super::ApiError::Internal)));
    assert!(rx.try_recv().is_err(), "nothing may reach the sequencer");
}

/// A plain HTTP/1.1 GET with its response head, failing the test after ten seconds.
async fn get(addr: SocketAddr, path: &str) -> (u16, String, Vec<u8>) {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    conn.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(10), conn.read_to_end(&mut response)).await.expect("a response in time").unwrap();
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").expect("headers end");
    let head = String::from_utf8_lossy(&response[..split]).to_lowercase();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, head, response[split + 4..].to_vec())
}
