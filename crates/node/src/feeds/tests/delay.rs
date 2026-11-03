//! A connection's pings against a loopback venue that answers them or does not: what counts as a
//! quote before and after an answer.

use std::sync::Arc;
use std::time::Duration;

use engine_core::types::MarketId;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use crate::clock::now_ms;
use crate::config::{ApiKey, Venue, VenueConnection, VenueSymbol};
use crate::feeds::kraken;
use crate::feeds::tests::fake::{
    Conn, WAIT, accept, fast, kraken_book, listen, next_alert, next_prices, send, spawn_feed, spawn_feed_into, stream,
};
use crate::feeds::tests::vendors::{KEY, Prices, serve_databento};
use crate::feeds::venue::{self, Timing};
use crate::sequencer::Request;
use crate::test_support::capture_alerts;
use crate::trusted_time::tests::logs::Logs;

const MARKETS: [(MarketId, &str); 1] = [(1, "BTC/USD")];
/// Short enough that a venue going quiet plays out in the test, long enough for a busy runner.
const TIMING: Timing = Timing {
    interval: Duration::from_millis(10),
    timeout: Duration::from_secs(1),
    reconnect_min: Duration::from_millis(10),
    reconnect_max: Duration::from_millis(40),
    stale_after: Duration::from_secs(1),
    ping_interval: Duration::from_millis(50),
    max_delay: Duration::from_millis(300),
};

#[tokio::test]
async fn the_first_ping_goes_before_the_opening_messages() {
    let (listener, url) = listen().await;
    let (_rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, TIMING);
    let mut ws = handshake(&listener).await;
    assert!(matches!(next(&mut ws).await, Message::Ping(_)));
    assert_eq!(next(&mut ws).await, Message::text(kraken::subscriptions(&["BTC/USD"])[0].as_str()));
}

#[tokio::test]
async fn frames_from_a_venue_that_never_answers_count_as_no_quote_and_end_in_a_new_connection() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, TIMING);
    let mut ws = handshake(&listener).await;

    // The venue sends frames but never reads, so no ping is answered.
    let frames = tokio::spawn(async move {
        loop {
            let book = kraken_book("BTC/USD", "100", "101", now_ms());
            if ws.send(Message::text(book)).await.is_err() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    });
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_delayed", "kraken"));
    assert!(rx.try_recv().is_err(), "a frame nothing bounds reached the sequencer");
    // The connection is replaced, and no quote reached the sequencer on the way.
    timeout(WAIT, listener.accept()).await.expect("a new connection").expect("accept");
    frames.abort();
    assert!(rx.try_recv().is_err(), "a frame nothing bounds reached the sequencer");
}

#[tokio::test]
async fn an_answer_lets_frames_count_and_resolves_the_alert() {
    let (logs, _guard) = Logs::capture();
    let (listener, url) = listen().await;
    let timing = Timing { timeout: WAIT, ..TIMING };
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, timing);
    let mut ws = handshake(&listener).await;

    // Frames sent ahead of any answer are refused, and once the answer is overdue, alerted. Nothing
    // has read the feed's pings.
    while logs.warnings("venue_delayed") == 0 {
        ws.send(Message::text(kraken_book("BTC/USD", "100", "101", now_ms()))).await.expect("send");
        sleep(Duration::from_millis(10)).await;
    }
    assert!(rx.try_recv().is_err(), "a frame nothing bounds reached the sequencer");

    // Reading answers the pings waiting, and frames count from then on.
    let books = stream(ws, || vec![kraken_book("BTC/USD", "100", "101", now_ms())]);
    assert_eq!(next_prices(&mut rx).await.0, 1);
    assert_eq!(logs.count("alert_resolved=\"venue_delayed\""), 1);
    books.abort();
}

#[tokio::test]
async fn a_stream_briefly_past_the_bound_refuses_frames_without_an_alert() {
    let (logs, _guard) = Logs::capture();
    let (listener, url) = listen().await;
    let max_delay = Duration::from_secs(2);
    let timing = Timing { timeout: WAIT, max_delay, ..TIMING };
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, timing);
    // Accepting answers the opening ping. The venue then sends without reading, so no later ping is
    // answered and frames are refused once that answer is past the bound, for far less than it.
    let (mut ws, _) = accept(&listener).await;
    let refused_for = Duration::from_millis(300);
    let started = tokio::time::Instant::now();
    while started.elapsed() < max_delay + refused_for {
        ws.send(Message::text(kraken_book("BTC/USD", "100", "101", now_ms()))).await.expect("send");
        sleep(Duration::from_millis(50)).await;
    }
    // Reading answers the pings waiting, so frames count again, and none of this was alerted.
    while rx.try_recv().is_ok() {}
    let books = stream(ws, || vec![kraken_book("BTC/USD", "100", "101", now_ms())]);
    assert_eq!(next_prices(&mut rx).await.0, 1);
    assert_eq!(logs.warnings("venue_delayed"), 0);
    books.abort();
}

#[tokio::test]
async fn a_quote_carries_when_the_ping_that_bounds_it_runs_out() {
    let (listener, url) = listen().await;
    let max_delay = Duration::from_secs(2);
    let timing = Timing { max_delay, ..fast(WAIT) };
    let (tx, mut rx) = mpsc::channel(1024);
    let before = std::time::Instant::now();
    let _feed = spawn_feed_into(tx, Venue::Kraken, &url, &MARKETS, timing);
    let (mut ws, _) = accept(&listener).await;
    // One quote as the host clock reads, then one ahead of it, which waits for the clock.
    let ahead = now_ms() + 300;
    let frames = [kraken_book("BTC/USD", "100", "101", now_ms()), kraken_book("BTC/USD", "100", "101", ahead)];
    send(&mut ws, &frames).await;
    for _ in 0..2 {
        let not_after = next_not_after(&mut rx).await.expect("a quote on a connection with pings");
        // The opening ping is the only one answered, and it was sent after `before`.
        assert!(not_after >= before + max_delay && not_after <= std::time::Instant::now() + max_delay);
    }
}

#[tokio::test]
async fn a_quote_on_a_connection_without_pings_carries_no_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("tcp://{}", listener.local_addr().expect("address"));
    let prices: Prices = Arc::new(|_, _| Some(("100".parse().unwrap(), "101".parse().unwrap())));
    let _venue = serve_databento(listener, prices);
    let (tx, mut rx) = mpsc::channel(1024);
    let symbols = vec![VenueSymbol { market: 1, symbol: "BTC".into(), contract: None }];
    let conn =
        VenueConnection { venue: Venue::Databento, url, api_key_env: None, dataset: Some("TEST".into()), symbols };
    let api_key = ApiKey::from_env("KEY", |_| Some(KEY.to_owned())).expect("a key");
    let tls = venue::tls_config().expect("tls config");
    let _feed = tokio::spawn(venue::run(conn, Some(api_key), TIMING, tls, tx));
    assert_eq!(next_not_after(&mut rx).await, None);
}

// ── Helpers ──

/// The `not_after` of the next quote the feed sequences.
async fn next_not_after(rx: &mut mpsc::Receiver<Request>) -> Option<std::time::Instant> {
    let quote = async {
        loop {
            if let Request::Quote { not_after, .. } = rx.recv().await.expect("feed running") {
                return not_after;
            }
        }
    };
    timeout(WAIT, quote).await.expect("a quote in time")
}

/// Accepts the next connection and completes the WebSocket handshake, without reading further.
async fn handshake(listener: &TcpListener) -> Conn {
    let (tcp, _) = timeout(WAIT, listener.accept()).await.expect("no connection in time").expect("accept");
    accept_async(tcp).await.expect("handshake")
}

/// The next message from the feed. Reading a ping queues its answer, which the next read or write
/// sends.
async fn next(ws: &mut Conn) -> Message {
    timeout(WAIT, ws.next()).await.expect("no message in time").expect("closed").expect("read")
}
