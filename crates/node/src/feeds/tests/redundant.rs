//! A venue read on two connections keeps its market priced through either one's failure: with
//! Coinbase on Advanced Trade and Exchange, and Kraken, BTC-USD stays priced while the Advanced
//! Trade connection serves a stale replay, stalls, or drops.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use engine_core::price::{Index, PriceError, compute_index};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::accept_async;

use crate::clock::now_ms;
use crate::config::Venue;
use crate::feeds::tests::fake::{
    WAIT, accept, coinbase_exchange_book, coinbase_levels, fake_sequencer, fast, kraken_book, listen, spawn_feed_into,
    stream,
};
use crate::sequencer::{Query, Request};

/// How the Advanced Trade connection fails.
#[derive(Debug, Clone, Copy)]
enum Failure {
    /// Every book it sends is ten minutes old, as Coinbase served one product on 16 November 2026.
    StaleReplay,
    /// It accepts the connection and sends nothing, nor answers a ping.
    Stalled,
    /// It drops every connection as soon as it is made.
    Dropped,
}

/// BTC-USD's index in the engine the feeds sequence into, judged at the host clock or the engine's
/// time, whichever is later, so a gap in what is sequenced shows.
async fn index(tx: &mpsc::Sender<Request>) -> Result<Index, PriceError> {
    let (reply, answer) = oneshot::channel();
    let query: Query = Box::new(move |engine| {
        let state = &engine.markets()[&1];
        let held = state.quotes_for(None).expect("BTC-USD takes quotes");
        let _ = reply.send(compute_index(held, &state.spec.price_rules, now_ms().max(engine.now())));
    });
    tx.send(Request::Query(query)).await.expect("the fake sequencer");
    timeout(WAIT, answer).await.expect("an answer in time").expect("answered")
}

/// Serves the failing Advanced Trade endpoint, every connection failing as `failure` says, until
/// the test ends.
fn fail(listener: TcpListener, failure: Failure) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stalled = Vec::new();
        loop {
            let Ok((tcp, _)) = listener.accept().await else { return };
            match failure {
                Failure::StaleReplay => {
                    let ws = accept_async(tcp).await.expect("handshake");
                    let sequence = AtomicU64::new(0);
                    stream(ws, move || {
                        let sequence = sequence.fetch_add(1, Ordering::Relaxed);
                        vec![coinbase_levels(sequence, true, "BTC-USD", "100", "101", now_ms() - 600_000)]
                    });
                }
                // Never read, so its pings go unanswered too.
                Failure::Stalled => stalled.push(accept_async(tcp).await.expect("handshake")),
                Failure::Dropped => drop(tcp),
            }
        }
    })
}

#[tokio::test]
async fn a_market_stays_priced_while_one_of_its_venues_two_connections_fails() {
    for failure in [Failure::StaleReplay, Failure::Stalled, Failure::Dropped] {
        let (tx, mut rx) = fake_sequencer();
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let (advanced, exchange, kraken) = (listen().await, listen().await, listen().await);
        let timing = fast(Duration::from_secs(1));
        let feeds = [
            spawn_feed_into(tx.clone(), Venue::Coinbase, &advanced.1, &[(1, "BTC-USD")], timing),
            spawn_feed_into(tx.clone(), Venue::CoinbaseExchange, &exchange.1, &[(1, "BTC-USD")], timing),
            spawn_feed_into(tx.clone(), Venue::Kraken, &kraken.1, &[(1, "BTC/USD")], timing),
        ];
        let failing = fail(advanced.0, failure);
        let (exchange, _) = accept(&exchange.0).await;
        stream(exchange, || vec![coinbase_exchange_book("BTC-USD", "100", "101", now_ms())]);
        let (kraken, _) = accept(&kraken.0).await;
        stream(kraken, || vec![kraken_book("BTC/USD", "100", "101", now_ms())]);

        let priced = async { while index(&tx).await.is_err() {} };
        timeout(WAIT, priced).await.unwrap_or_else(|_| panic!("{failure:?}: never priced"));
        // Through three of the failing connection's reconnects, each a second of silence at most.
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            assert_eq!(index(&tx).await.map(|index| index.sources), Ok(2), "{failure:?}");
            sleep(Duration::from_millis(100)).await;
        }
        failing.abort();
        feeds.iter().for_each(JoinHandle::abort);
    }
}
