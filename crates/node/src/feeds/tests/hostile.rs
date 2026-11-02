//! A hostile host against the real sequencer and venue tasks: its clock behind true time by as much
//! as the drift bound admits, and each venue's bytes held back by the same, in order, as a host on
//! the path of a TLS stream can. Every freshness check on the wall clock passes, so only a
//! connection's pings keep the delay from pricing the market.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use engine_core::Fixed;
use engine_core::event::{Event, Reject};
use engine_core::price::PriceError;
use engine_core::types::{MarketId, Millis};
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until, timeout};

use crate::clock::now_ms;
use crate::config::Venue;
use crate::feeds::tests::fake::{
    Conn, WAIT, accept, bitstamp_book, fast, kraken_book, listen, spawn_feed_into, stream,
};
use crate::feeds::tests::scenario::Node;
use crate::feeds::venue::Timing;
use crate::sequencer::Request;
use crate::test_support::{LOADED_WAIT, capture_alerts, dev_config};
use crate::trusted_time::Sample;

/// What the venues quote until they move, and after.
const BEFORE: (&str, &str, &str) = ("81250", "81251", "81250.5");
const AFTER: (&str, &str, &str) = ("81350", "81351", "81350.5");
const MAX_AGE: Duration = Duration::from_secs(1);
const PING_INTERVAL: Duration = Duration::from_millis(100);
/// The stated bound's `feeds.venues.max_delay`, as the node's dev config sets it.
const MAX_DELAY: Duration = Duration::from_secs(2);

#[tokio::test]
async fn past_the_delay_bound_the_market_goes_unpriced_with_the_clock_at_the_edge_of_the_drift_bound() {
    let (_guard, mut alerts) = capture_alerts();
    let lag = MAX_DELAY + Duration::from_millis(300);
    let (mut node, _moved) = hostile_node("past-the-bound", lag).await;

    // The connection's first pong comes back `lag` late, and each frame behind it no sooner.
    let delayed = async {
        let mut venues = Vec::new();
        while venues.len() < 2 {
            let alert = alerts.recv().await.expect("alert channel open");
            assert_ne!(alert["alert"], "clock_drift", "the host clock is within the drift bound: {alert:?}");
            if alert["alert"] == "venue_delayed" {
                venues.push(alert["venue"].clone());
            }
        }
        venues.sort();
        venues
    };
    let venues = timeout(WAIT + lag * 2, delayed).await.expect("both connections alerted");
    assert_eq!(venues, ["bitstamp", "kraken"]);

    // Every check on the wall clock would pass, so only the bound keeps these frames out. The
    // sequencer still stamps at this clock: the refusal below is the engine's, not the time gate's.
    assert_eq!(node.prices_within(1, Duration::from_millis(500)).await, 0, "no delayed quote reaches the engine");
    node.deposit().await;
    let events = node.buy(1, BEFORE.2).await;
    let stale = Reject::NoPrice { error: PriceError::Stale { fresh: 0, needed: 2 } };
    assert!(
        events.iter().any(|event| matches!(event, Event::OrderRejected { reason, .. } if *reason == stale)),
        "{events:?}"
    );
}

#[tokio::test]
async fn within_the_delay_bound_a_delayed_price_is_no_older_than_the_stated_bound() {
    let lag = MAX_DELAY - Duration::from_millis(500);
    let (mut node, moved) = hostile_node("within-the-bound", lag).await;
    let lag_ms = lag.as_millis() as Millis;
    node.wait_for_quotes_within(&[(1, "kraken"), (1, "bitstamp")], LOADED_WAIT).await;
    node.deposit().await;
    let bound_ms = (MAX_DELAY + MAX_AGE.mul_f64(1.01)).as_millis() as Millis;

    // The venues move now, in true time: the host trades at what it has held back, for `lag`.
    moved.store(true, Ordering::SeqCst);
    let moved_at = Instant::now();
    assert_eq!(node.fill_index(BEFORE.2).await, BEFORE.2.parse().unwrap());
    let age_ms = now_ms() + lag_ms - node.newest_quote(1).await;
    assert!(age_ms >= lag_ms - 100, "the host holds the price back by {lag_ms}ms, but it is only {age_ms}ms old");
    assert!(age_ms <= bound_ms, "a price {age_ms}ms old, past the stated {bound_ms}ms");

    // Once the frames the host held have arrived, the engine prices from the venues' new mid.
    sleep_until(moved_at + lag + Duration::from_millis(200)).await;
    assert_eq!(node.fill_index(AFTER.2).await, AFTER.2.parse().unwrap());
}

// ── Helpers ──

/// A sequencer whose host clock is `lag` behind true time, at the far edge of the drift bound, with
/// Kraken and Bitstamp venues stamping BTC-USD at true time behind a host holding what they send
/// back by `lag`. Returns the node and the switch that moves the venues' price.
async fn hostile_node(name: &str, lag: Duration) -> (Node, Arc<AtomicBool>) {
    let lag_ms = lag.as_millis() as Millis;
    let mut cfg = dev_config();
    for market in cfg.genesis.markets.iter_mut().filter(|market| market.id == 1) {
        market.price_rules.max_age_ms = MAX_AGE.as_millis() as Millis;
    }
    // True time is the top of the sample, so the host clock is as far behind it as the gate admits,
    // with room for the two clocks not being read at one instant.
    let max_drift = lag + Duration::from_millis(100);
    let latest_ms = now_ms() + lag_ms;
    let sample = Sample { at: std::time::Instant::now(), earliest_ms: latest_ms - 1_000, latest_ms };
    let node = Node::boot_lagged(name, &cfg.genesis, max_drift, sample).await;

    let moved = Arc::new(AtomicBool::new(false));
    let timing = Timing { ping_interval: PING_INTERVAL, max_delay: MAX_DELAY, ..fast(LOADED_WAIT) };
    let books = |book: fn(&str, &str, &str, Millis) -> String, symbol: &'static str| {
        let moved = moved.clone();
        move |ws: Conn| {
            stream(ws, move || {
                let (bid, ask, _) = if moved.load(Ordering::SeqCst) { AFTER } else { BEFORE };
                vec![book(symbol, bid, ask, now_ms() + lag_ms)]
            })
        }
    };
    let kraken = serve_held_back(lag, books(kraken_book, "BTC/USD")).await;
    spawn_feed_into(node.tx.clone(), Venue::Kraken, &kraken, &[(1, "BTC/USD")], timing);
    let bitstamp = serve_held_back(lag, books(bitstamp_book, "btcusd")).await;
    spawn_feed_into(node.tx.clone(), Venue::Bitstamp, &bitstamp, &[(1, "btcusd")], timing);
    (node, moved)
}

/// Starts a loopback venue that `serve` runs on its first connection, behind a host holding what it
/// sends back by `held`, and returns the URL that reaches it through that host.
async fn serve_held_back(held: Duration, serve: impl FnOnce(Conn) -> JoinHandle<()> + Send + 'static) -> String {
    let (venue, venue_url) = listen().await;
    tokio::spawn(async move {
        let (ws, _) = accept(&venue).await;
        let _ = serve(ws).await;
    });
    hold_back(venue_url.trim_start_matches("ws://").to_owned(), held).await
}

/// A host on the path to `venue`: what the feed sends passes at once, and what the venue sends
/// passes `held` later, in order. Returns the `ws://` URL that reaches the venue through it.
async fn hold_back(venue: String, held: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let url = format!("ws://{}", listener.local_addr().expect("local addr"));
    tokio::spawn(async move {
        let (feed, _) = listener.accept().await.expect("accept");
        let (mut feed_read, mut feed_write) = feed.into_split();
        let (mut venue_read, mut venue_write) =
            TcpStream::connect(&venue).await.expect("connect to the venue").into_split();
        tokio::spawn(async move {
            let _ = copy(&mut feed_read, &mut venue_write).await;
        });
        let (held_tx, mut held_rx) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
        tokio::spawn(async move {
            let mut buf = vec![0; 64 * 1024];
            while let Ok(read @ 1..) = venue_read.read(&mut buf).await {
                if held_tx.send((Instant::now() + held, buf[..read].to_vec())).is_err() {
                    return;
                }
            }
        });
        while let Some((due, bytes)) = held_rx.recv().await {
            sleep_until(due).await;
            if feed_write.write_all(&bytes).await.is_err() {
                return;
            }
        }
    });
    url
}

impl Node {
    /// The newest stamp among the quotes the engine holds for `market`.
    async fn newest_quote(&self, market: MarketId) -> Millis {
        let (reply, answer) = oneshot::channel();
        let query = Box::new(move |engine: &engine_core::Engine| {
            let newest = engine.markets()[&market].quotes.values().map(|quote| quote.ts).max();
            let _ = reply.send(newest.expect("a quote held"));
        });
        self.tx.send(Request::Query(query)).await.expect("sequencer up");
        answer.await.expect("an answer")
    }

    /// The index a small BTC-USD buy quoted at `mid` fills at, retrying while the quotes change hands.
    async fn fill_index(&self, mid: &str) -> Fixed {
        for _ in 0..50 {
            let events = self.buy(1, mid).await;
            if let Some(fill) = events.iter().find_map(|e| if let Event::Fill(fill) = e { Some(fill) } else { None }) {
                return fill.index;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("the market has no price to fill at");
    }
}
