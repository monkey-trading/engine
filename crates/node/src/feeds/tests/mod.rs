//! Venue feed tasks against loopback venues: what they sequence, what they refuse, and how they
//! reconnect.

pub(super) mod fake;
mod hours;
mod late;
mod mock;
mod rwa;
pub(super) mod rwa_support;
mod scenario;
mod vendors;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use engine_core::types::MarketId;
use futures_util::SinkExt;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;

use crate::clock::now_ms;
use crate::config::Venue;
use crate::feeds::tests::fake::{
    WAIT, accept, bitstamp_book, coinbase_levels, fast, kraken_book, listen, next_alert, next_prices, quote, send,
    spawn_feed, stream,
};
use crate::feeds::venue::{Book, Instrument, Timing, to_quote};
use crate::feeds::{bitstamp, kraken};
use crate::test_support::{Alert, capture_alerts};

const MARKETS: [(MarketId, &str); 2] = [(1, "BTC/USD"), (2, "ETH/USD")];

#[tokio::test]
async fn quotes_are_sequenced_per_market_with_the_venue_timestamp() {
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(WAIT));
    let (mut ws, subscription) = accept(&listener).await;
    assert_eq!(subscription, kraken::subscriptions(&["BTC/USD", "ETH/USD"])[0]);

    send(
        &mut ws,
        &[kraken_book("BTC/USD", "81250.5", "81251.5", 1_000), kraken_book("ETH/USD", "2377", "2377.1", 2_000)],
    )
    .await;
    let mut sequenced = BTreeMap::from([next_prices(&mut rx).await, next_prices(&mut rx).await]);
    assert_eq!(sequenced.remove(&1), Some(vec![quote(Venue::Kraken, "81251", 1_000)]));
    assert_eq!(sequenced.remove(&2), Some(vec![quote(Venue::Kraken, "2377.05", 2_000)]));
}

#[tokio::test]
async fn only_the_newest_quote_per_market_is_sequenced_each_interval() {
    let (listener, url) = listen().await;
    // A wide interval, so all three land in it even on a busy runner.
    let timing = Timing { interval: Duration::from_secs(3), ..fast(WAIT) };
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, timing);
    let (mut ws, _) = accept(&listener).await;

    // All three arrive within one interval, the newest by venue time second, so only it is sequenced.
    let tickers = [1_000, 1_200, 1_100].map(|ts| kraken_book("BTC/USD", "100", "101", ts));
    send(&mut ws, &tickers).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "100.5", 1_200)]));
}

#[tokio::test]
async fn malformed_messages_and_books_are_refused_with_an_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    // Every message lands within one interval, so a refused book accepted after the valid one
    // would replace it as the quote sequenced. Bitstamp keeps no book, so the connection carries on.
    let timing = Timing { interval: Duration::from_secs(1), ..fast(WAIT) };
    let (mut rx, _feed) = spawn_feed(Venue::Bitstamp, &url, &[(1, "btcusd"), (2, "ethusd")], timing);
    let (mut ws, _) = accept(&listener).await;

    let messages = [
        bitstamp_book("btcusd", "100", "101", 1_000),
        "not json".to_owned(),
        bitstamp_book("btcusd", "101", "100", 2_000),
        bitstamp_book("btcusd", "0", "100", 2_000),
        bitstamp_book("btcusd", "100", "1000000001", 2_000),
        bitstamp_book("solusd", "100", "101", 2_000),
    ];
    send(&mut ws, &messages).await;

    // Nothing refused reaches the sequencer, and the first refusal alerts.
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Bitstamp, "100.5", 1_000)]));
    let alert = next_alert(&mut alerts).await;
    assert_eq!(alert["alert"], "venue_message_refused");
    assert_eq!(alert["venue"], "bitstamp");
    // The other refusals within the re-alert window are counted, not alerted.
    assert!(alerts.try_recv().is_err());
}

#[tokio::test]
async fn a_dropped_connection_reconnects_and_quotes_resume_without_an_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(WAIT));

    let (mut first, _) = accept(&listener).await;
    let ts = now_ms();
    send(&mut first, &[kraken_book("BTC/USD", "100", "101", ts)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "100.5", ts)]));
    drop(first);

    let (mut second, subscription) = accept(&listener).await;
    assert_eq!(subscription, kraken::subscriptions(&["BTC/USD", "ETH/USD"])[0]);
    let ts = now_ms();
    send(&mut second, &[kraken_book("BTC/USD", "100", "102", ts)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "101", ts)]));
    // A connection that had sequenced fresh quotes is a routine drop, not an outage.
    assert!(alerts.try_recv().is_err());
}

#[tokio::test]
async fn a_handshake_that_never_completes_times_out_and_alerts() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (_rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(Duration::from_millis(200)));

    // The first attempt reaches TCP, which is held open without answering the handshake.
    let (_held, _) = timeout(WAIT, listener.accept()).await.expect("no attempt in time").expect("accept");
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_feed_down", "kraken"));
    timeout(WAIT, listener.accept()).await.expect("no retry after the timeout").expect("accept");
}

#[tokio::test]
async fn an_unreachable_venue_alerts_once_and_keeps_retrying() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (_rx, feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(WAIT));

    // Each attempt reaches TCP and is cut before the WebSocket handshake.
    for _ in 0..3 {
        let (tcp, _) = timeout(WAIT, listener.accept()).await.expect("no attempt in time").expect("accept");
        drop(tcp);
    }
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_feed_down", "kraken"));
    assert!(alerts.try_recv().is_err(), "the outage alerted more than once");
    assert!(!feed.is_finished());
}

#[tokio::test]
async fn a_connection_with_no_fresh_quote_is_replaced_even_while_down() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (_rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(Duration::from_millis(200)));
    drop(timeout(WAIT, listener.accept()).await.expect("no attempt in time"));
    assert_eq!(next_alert(&mut alerts).await["alert"], "venue_feed_down");

    // The subscribe is refused and only heartbeats follow: the connection is alive but useless.
    let (mut ws, _) = accept(&listener).await;
    let refused = r#"{"error":"Currency pair not supported BTC/USD","method":"subscribe","success":false}"#;
    send(&mut ws, &[refused.to_owned()]).await;
    stream(ws, || vec![r#"{"channel":"heartbeat"}"#.to_owned()]);
    let (_next, subscription) = accept(&listener).await;
    assert_eq!(subscription, kraken::subscriptions(&["BTC/USD", "ETH/USD"])[0]);
}

#[tokio::test]
async fn a_quote_too_far_ahead_for_the_engine_is_held_back_and_alerted() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], fast(WAIT));
    let (mut ws, _) = accept(&listener).await;

    // The engine refuses a quote stamped more than a second past its time, so the feed does not
    // sequence one, and says why.
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms() + 60_000)]).await;
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_clock_ahead", "kraken"));
    // The next, correctly dated quote is the first one sequenced.
    let ts = now_ms();
    send(&mut ws, &[kraken_book("BTC/USD", "100", "102", ts)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "101", ts)]));
}

#[tokio::test]
async fn a_binary_message_is_refused_with_an_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(WAIT));
    let (mut ws, _) = accept(&listener).await;
    ws.send(Message::binary(vec![1, 2, 3])).await.expect("send");
    let ts = now_ms();
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", ts)]).await;

    let alert = next_alert(&mut alerts).await;
    assert_eq!(alert["alert"].as_str(), "venue_message_refused");
    assert!(alert["message"].contains("binary message"), "{alert:?}");
    // The connection carries on.
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "100.5", ts)]));
}

#[tokio::test]
async fn a_symbol_the_venue_never_quotes_is_alerted_and_resubscribed() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(Duration::from_millis(300)));
    let (ws, _) = accept(&listener).await;

    // BTC keeps quoting fresh while ETH never quotes.
    stream(ws, || vec![kraken_book("BTC/USD", "100", "101", now_ms())]);
    next_prices(&mut rx).await;
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["symbol"].as_str()), ("venue_symbol_silent", "ETH/USD"));
    // The feed replaces the connection to subscribe ETH afresh.
    let (_ws, subscription) = accept(&listener).await;
    assert_eq!(subscription, kraken::subscriptions(&["BTC/USD", "ETH/USD"])[0]);
}

#[tokio::test]
async fn stale_quotes_do_not_count_as_the_venue_quoting() {
    // A stuck venue repeats a book a minute old; the engine still gets it and judges its age.
    let stuck = now_ms() - 60_000;
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], fast(Duration::from_millis(300)));
    let (ws, _) = accept(&listener).await;
    stream(ws, move || vec![kraken_book("BTC/USD", "100", "101", stuck)]);
    let (market, quotes) = next_prices(&mut rx).await;
    assert_eq!((market, quotes[0].source.as_str()), (1, "kraken"));
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["symbol"].as_str()), ("venue_symbol_silent", "BTC/USD"));
}

#[tokio::test]
async fn quotes_a_little_ahead_of_the_host_clock_are_sequenced_once_it_reaches_them() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let window = Duration::from_millis(500);
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], fast(window));
    let (ws, _) = accept(&listener).await;

    // Each quote is dated further ahead than the sequencing interval, so every tick also holds
    // newer quotes not yet reached. Each goes once reached, and the venue counts as quoting.
    stream(ws, || vec![kraken_book("BTC/USD", "100", "101", now_ms() + 100)]);
    let (_, quotes) = next_prices(&mut rx).await;
    assert!(quotes[0].ts <= now_ms(), "sequenced before the host clock reached it");
    sleep(window * 3).await;
    assert!(alerts.try_recv().is_err(), "the venue was alerted");
}

#[tokio::test]
async fn each_outage_alerts_once() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(WAIT));
    let refuse_handshake = async || drop(timeout(WAIT, listener.accept()).await.expect("no attempt in time"));

    refuse_handshake().await;
    assert_eq!(next_alert(&mut alerts).await["alert"], "venue_feed_down");
    // A fresh quote ends the outage; the connection then drops and the venue stays down.
    let (mut ws, _) = accept(&listener).await;
    let ts = now_ms();
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", ts)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "100.5", ts)]));
    drop(ws);
    refuse_handshake().await;
    assert_eq!(next_alert(&mut alerts).await["alert"], "venue_feed_down");
}

#[tokio::test]
async fn an_outage_alert_stands_for_every_symbol_until_a_fresh_quote() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let silence = Duration::from_millis(200);
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(silence));
    drop(timeout(WAIT, listener.accept()).await.expect("no attempt in time"));
    assert_eq!(next_alert(&mut alerts).await["alert"], "venue_feed_down");

    // Back, but quoting only books a minute old: still down, so no symbol alerts on top of it.
    let (ws, _) = accept(&listener).await;
    stream(ws, || vec![kraken_book("BTC/USD", "100", "101", now_ms() - 60_000)]);
    next_prices(&mut rx).await;
    assert!(timeout(silence * 3, alerts.recv()).await.is_err(), "alerted again during the outage");
}

#[tokio::test]
async fn a_symbol_that_goes_quiet_again_alerts_again() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(Duration::from_millis(300)));
    let eth_quoting = Arc::new(AtomicBool::new(false));
    let eth = Arc::clone(&eth_quoting);
    // Every connection quotes BTC, and ETH while the switch is on.
    let serving = tokio::spawn(async move {
        loop {
            let (ws, _) = accept(&listener).await;
            let eth = Arc::clone(&eth);
            let messages = move || {
                let ts = now_ms();
                let eth = eth.load(Ordering::Relaxed).then(|| kraken_book("ETH/USD", "10", "11", ts));
                [Some(kraken_book("BTC/USD", "100", "101", ts)), eth].into_iter().flatten().collect()
            };
            let _ = stream(ws, messages).await;
        }
    });

    let silent_eth = |alert: &Alert| alert["alert"] == "venue_symbol_silent" && alert["symbol"] == "ETH/USD";
    assert!(silent_eth(&next_alert(&mut alerts).await));
    eth_quoting.store(true, Ordering::Relaxed);
    let eth_sequenced = async { while next_prices(&mut rx).await.0 != 2 {} };
    timeout(WAIT, eth_sequenced).await.expect("ETH quoted in time");
    eth_quoting.store(false, Ordering::Relaxed);
    assert!(silent_eth(&next_alert(&mut alerts).await));
    serving.abort();
}

#[tokio::test]
async fn silence_is_counted_across_reconnects() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (_rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(Duration::from_millis(300)));
    // Each connection quotes BTC for about 100ms and drops, shorter than the silence bound; ETH
    // never quotes.
    let flapping = tokio::spawn(async move {
        loop {
            let (mut ws, _) = accept(&listener).await;
            for _ in 0..5 {
                send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
                sleep(Duration::from_millis(20)).await;
            }
        }
    });
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["symbol"].as_str()), ("venue_symbol_silent", "ETH/USD"));
    flapping.abort();
}

#[tokio::test]
async fn a_book_that_falls_out_of_sync_resyncs_with_an_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Coinbase, &url, &[(1, "BTC-USD")], fast(WAIT));

    let (mut first, _) = accept(&listener).await;
    send(&mut first, &[coinbase_levels(0, true, "BTC-USD", "100", "101", 1_000)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Coinbase, "100.5", 1_000)]));
    // Sequence 1 never arrives.
    send(&mut first, &[coinbase_levels(2, false, "BTC-USD", "100.5", "101", 2_000)]).await;

    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_message_refused", "coinbase"));
    let (mut second, _) = accept(&listener).await;
    send(&mut second, &[coinbase_levels(0, true, "BTC-USD", "100", "102", 3_000)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Coinbase, "101", 3_000)]));
}

#[tokio::test]
async fn a_refused_book_from_a_kept_book_resyncs() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Coinbase, &url, &[(1, "BTC-USD")], fast(WAIT));

    let (mut first, _) = accept(&listener).await;
    send(&mut first, &[coinbase_levels(0, true, "BTC-USD", "100", "101", 1_000)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Coinbase, "100.5", 1_000)]));
    // In sequence and well formed, but its bid crosses the kept offer: the kept book is suspect.
    send(&mut first, &[coinbase_levels(1, false, "BTC-USD", "101.5", "102", now_ms())]).await;

    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_message_refused", "coinbase"));
    let (mut second, _) = accept(&listener).await;
    send(&mut second, &[coinbase_levels(0, true, "BTC-USD", "100", "102", 3_000)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Coinbase, "101", 3_000)]));
}

#[tokio::test]
async fn a_kraken_book_that_fails_verification_resubscribes_without_a_quote() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], fast(WAIT));

    let (mut first, _) = accept(&listener).await;
    let corrupted = kraken_book("BTC/USD", "100", "101", 1_000).replace(r#""qty":1.50000000"#, r#""qty":1.50000001"#);
    send(&mut first, &[corrupted]).await;
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_message_refused", "kraken"));
    assert!(alert["message"].contains("checksum mismatch"), "{alert:?}");

    // A book that verifies but is crossed is suspect too: it is dropped the same way.
    let (mut crossed, _) = accept(&listener).await;
    send(&mut crossed, &[kraken_book("BTC/USD", "101", "100", 1_500)]).await;

    // A fresh subscription brings a fresh snapshot, and only its top is sequenced.
    let (mut second, subscription) = accept(&listener).await;
    assert_eq!(subscription, kraken::subscriptions(&["BTC/USD"])[0]);
    send(&mut second, &[kraken_book("BTC/USD", "100", "102", 2_000)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "101", 2_000)]));
}

#[tokio::test]
async fn a_reconnect_request_ends_the_connection() {
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Bitstamp, &url, &[(1, "btcusd")], fast(WAIT));

    let (mut first, _) = accept(&listener).await;
    send(&mut first, &[r#"{"event":"bts:request_reconnect","channel":"","data":""}"#.to_owned()]).await;
    let (mut second, subscription) = accept(&listener).await;
    assert_eq!(subscription, bitstamp::subscriptions(&["btcusd"])[0]);
    send(&mut second, &[bitstamp_book("btcusd", "100", "101", 1_000)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Bitstamp, "100.5", 1_000)]));
}

#[tokio::test]
async fn the_feed_stops_when_the_sequencer_goes_away() {
    let (listener, url) = listen().await;
    let (rx, feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(WAIT));
    let (mut ws, _) = accept(&listener).await;
    drop(rx);
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", 1_000)]).await;

    let stopped = timeout(WAIT, feed).await.expect("feed kept running").expect("feed panicked");
    let err = format!("{:#}", stopped.unwrap_err());
    assert!(err.contains("sequencer gone"), "{err}");
    // The feed never reconnects once the sequencer is gone.
    assert!(timeout(Duration::from_millis(100), listener.accept()).await.is_err());
}

#[test]
fn books_no_working_venue_could_show_are_refused() {
    const NOW: i64 = 1_792_489_875_714;
    const DAY: i64 = 24 * 60 * 60 * 1_000;
    let markets = BTreeMap::from([("BTC/USD".to_owned(), Instrument { market: 1, contract: None })]);
    let book = |symbol: &str, bid: &str, ask: &str, ts| Book {
        symbol: symbol.into(),
        bid: bid.parse().unwrap(),
        ask: ask.parse().unwrap(),
        ts,
    };
    let refused = [
        ("unsubscribed symbol", book("SOL/USD", "100", "101", 1)),
        ("crossed", book("BTC/USD", "101", "100", 1)),
        ("crossed", book("BTC/USD", "100", "100", 1)),
        ("out of range", book("BTC/USD", "0", "100", 1)),
        ("out of range", book("BTC/USD", "100", "1000000000.1", 1)),
        ("before the epoch", book("BTC/USD", "100", "101", -1)),
        ("than any clock error", book("BTC/USD", "100", "101", NOW + DAY + 1)),
    ];
    for (expected, book) in refused {
        let err = to_quote(Venue::Kraken, &markets, book, NOW).unwrap_err();
        assert!(err.contains(expected), "expected {expected:?}, got {err}");
    }
    // A time up to a day ahead is a clock error, not a corrupt one, and passes; the mid truncates
    // toward zero at the ninth decimal.
    let accepted = to_quote(Venue::Kraken, &markets, book("BTC/USD", "100", "100.000000001", NOW + DAY), NOW);
    assert_eq!(accepted, Ok(("BTC/USD".to_owned(), quote(Venue::Kraken, "100", NOW + DAY))));
}
