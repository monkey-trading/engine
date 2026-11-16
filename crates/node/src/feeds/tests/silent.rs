//! Symbols a venue stops quoting usably, silent or streamed stale, resubscribed until they quote:
//! alone where the venue takes that, and on a new connection when that is not enough.

use std::time::Duration;

use engine_core::command::Command;
use engine_core::types::{MarketId, Millis};
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};

use crate::clock::now_ms;
use crate::config::Venue;
use crate::feeds::coinbase::{resubscriptions, subscriptions};
use crate::feeds::kraken;
use crate::feeds::tests::coinbase_support::{BTC, CoinbaseVenue, ETH};
use crate::feeds::tests::fake::{WAIT, accept, fast, kraken_book, listen, next_alert, spawn_feed, stream};
use crate::feeds::venue::{STALE_GRACE, Timing};
use crate::sequencer::Request;
use crate::test_support::{Alert, capture_alerts_and_resolutions};

const PRODUCTS: [(MarketId, &str); 2] = [(1, "BTC-USD"), (2, "ETH-USD")];

/// A source minutes behind Coinbase's own, as one product was served from on 16 November 2026.
const BEHIND: Duration = Duration::from_secs(600);

/// The node's delay bound and pings, a timeout past the stale grace, as the node's is, and short
/// reconnects.
fn coinbase_timing() -> Timing {
    Timing {
        interval: Duration::from_millis(10),
        timeout: Duration::from_secs(6),
        reconnect_min: Duration::from_millis(50),
        reconnect_max: Duration::from_millis(200),
        stale_after: Duration::from_secs(1),
        ping_interval: Duration::from_millis(500),
        max_delay: Duration::from_secs(2),
    }
}

/// The quotes a feed sequences, as each market and stamp, read off its sequencer channel so the
/// feed never waits on it.
fn sequenced(mut rx: mpsc::Receiver<Request>) -> mpsc::UnboundedReceiver<(MarketId, Millis)> {
    let (tx, quotes) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            if let Request::Submit { cmd: Command::Prices { market, quotes }, .. } = request {
                for quote in quotes {
                    let _ = tx.send((market, quote.ts));
                }
            }
        }
    });
    quotes
}

/// The next alert, or resolution, named `name` for `symbol`, passing over others.
async fn next_for(alerts: &mut mpsc::UnboundedReceiver<Alert>, key: &str, name: &str, symbol: &str) -> Alert {
    loop {
        let alert = next_alert(alerts).await;
        if alert[key] == name && alert["symbol"] == symbol {
            return alert;
        }
    }
}

/// Waits for a quote for `market` stamped within a second of the host clock, failing after `WAIT`
/// or when the feed stops.
async fn fresh_quote(quotes: &mut mpsc::UnboundedReceiver<(MarketId, Millis)>, market: MarketId) {
    let fresh = async {
        loop {
            let (quoted, ts) = quotes.recv().await.expect("the feed stopped");
            if quoted == market && now_ms() - ts <= 1_000 {
                return;
            }
        }
    };
    timeout(WAIT, fresh).await.expect("no fresh quote in time");
}

/// How old, in seconds, a `venue_stale` alert says the venue's quotes were when sent.
fn stale_age(alert: &Alert) -> f64 {
    let message = &alert["message"];
    let age = message.strip_prefix("venue quotes ").and_then(|rest| rest.split_once("s old when sent"));
    age.and_then(|(age, _)| age.parse().ok()).unwrap_or_else(|| panic!("no age in {message:?}"))
}

/// The messages a resubscription of ETH-USD alone sends.
fn eth_alone() -> Vec<String> {
    resubscriptions(&["ETH-USD"])
}

#[tokio::test]
async fn a_product_streamed_stale_is_resubscribed_alone_while_the_other_quotes_on() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let mut venue = CoinbaseVenue::start(&[BTC, ETH]).await;
    venue.behind(ETH.id, &[BEHIND]);
    let (rx, _feed) = spawn_feed(Venue::Coinbase, &venue.url, &PRODUCTS, coinbase_timing());
    let mut quotes = sequenced(rx);
    assert_eq!(venue.next_received().await, (0, subscriptions(&["BTC-USD", "ETH-USD"])[0].clone()));
    let subscribed = Instant::now();
    fresh_quote(&mut quotes, 1).await;

    // ETH reads ten minutes old: the cause is alerted once its reads span the grace, well inside
    // the silence timeout (the margin covers reading both snapshots in a debug build), and ETH alone
    // is subscribed afresh.
    let stale = next_for(&mut alerts, "alert", "venue_stale", "ETH-USD").await;
    assert!(subscribed.elapsed() < STALE_GRACE + Duration::from_secs(2), "detected after {:?}", subscribed.elapsed());
    // Ten minutes, less how far behind the stream's own pings put it.
    assert!((599.0..=601.0).contains(&stale_age(&stale)), "{stale:?}");
    let resubscribed = [venue.next_received().await, venue.next_received().await];
    assert_eq!(resubscribed, [(0, eth_alone()[0].clone()), (0, eth_alone()[1].clone())]);
    // Its fresh book ends the episode on the same connection, while BTC quotes on.
    next_for(&mut resolved, "alert_resolved", "venue_stale", "ETH-USD").await;
    fresh_quote(&mut quotes, 2).await;
    fresh_quote(&mut quotes, 1).await;
    assert_eq!(venue.drain(), [], "the feed sent more, or connected again");
}

#[tokio::test]
async fn each_stale_episode_on_a_connection_resubscribes_alone_first() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let mut venue = CoinbaseVenue::start(&[BTC, ETH]).await;
    venue.behind(ETH.id, &[BEHIND]);
    let (rx, _feed) = spawn_feed(Venue::Coinbase, &venue.url, &PRODUCTS, coinbase_timing());
    let mut quotes = sequenced(rx);
    assert_eq!(venue.next_received().await, (0, subscriptions(&["BTC-USD", "ETH-USD"])[0].clone()));

    // ETH's episode: resubscribed alone, and current again.
    let resubscribed = [venue.next_received().await, venue.next_received().await];
    assert_eq!(resubscribed, [(0, eth_alone()[0].clone()), (0, eth_alone()[1].clone())]);
    next_for(&mut resolved, "alert_resolved", "venue_stale", "ETH-USD").await;
    fresh_quote(&mut quotes, 2).await;
    // BTC's, later on the same connection, is resubscribed alone too, not by a new connection.
    venue.set_lag(BTC.id, BEHIND);
    next_for(&mut alerts, "alert", "venue_stale", "BTC-USD").await;
    let btc_alone = resubscriptions(&["BTC-USD"]);
    let resubscribed = [venue.next_received().await, venue.next_received().await];
    assert_eq!(resubscribed, [(0, btc_alone[0].clone()), (0, btc_alone[1].clone())]);
    next_for(&mut resolved, "alert_resolved", "venue_stale", "BTC-USD").await;
    fresh_quote(&mut quotes, 1).await;
    assert_eq!(venue.drain(), [], "the feed sent more, or connected again");
}

#[tokio::test]
async fn a_product_still_stale_after_resubscribing_alone_is_resubscribed_on_a_new_connection() {
    let (_guard, _alerts, mut resolved) = capture_alerts_and_resolutions();
    let mut venue = CoinbaseVenue::start(&[BTC, ETH]).await;
    // Stale on the first connection, alone on it, and on the next connection; current alone on that.
    venue.behind(ETH.id, &[BEHIND, BEHIND, BEHIND]);
    let (rx, _feed) = spawn_feed(Venue::Coinbase, &venue.url, &PRODUCTS, coinbase_timing());
    let mut quotes = sequenced(rx);

    let both = subscriptions(&["BTC-USD", "ETH-USD"])[0].clone();
    let mut sent = Vec::new();
    for _ in 0..6 {
        sent.push(venue.next_received().await);
    }
    let [unsubscribe, subscribe] = [eth_alone()[0].clone(), eth_alone()[1].clone()];
    assert_eq!(
        sent,
        [
            (0, both.clone()),
            (0, unsubscribe.clone()),
            (0, subscribe.clone()),
            (1, both),
            (1, unsubscribe),
            (1, subscribe)
        ]
    );
    next_for(&mut resolved, "alert_resolved", "venue_symbol_silent", "ETH-USD").await;
    fresh_quote(&mut quotes, 2).await;
}

#[tokio::test]
async fn a_symbol_still_silent_on_a_new_connection_is_resubscribed_again() {
    let (listener, url) = listen().await;
    let (_rx, _feed) =
        spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD"), (2, "ETH/USD")], fast(Duration::from_millis(300)));
    let subscription = kraken::subscriptions(&["BTC/USD", "ETH/USD"])[0].clone();

    // BTC quotes on each connection and ETH never does. Kraken takes no resubscription of one
    // symbol alone, so each attempt replaces the connection, and the one after a reconnect too.
    for _ in 0..2 {
        let (ws, subscribed) = accept(&listener).await;
        assert_eq!(subscribed, subscription);
        stream(ws, || vec![kraken_book("BTC/USD", "100", "101", now_ms())]);
    }
    let (_third, subscribed) = accept(&listener).await;
    assert_eq!(subscribed, subscription);
}
