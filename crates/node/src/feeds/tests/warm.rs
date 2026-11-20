//! A feed opened before its node runs, its quotes held back until the node lets them through, against
//! a loopback Kraken that sends books and heartbeats and answers the opening ping.

use std::time::Duration;

use engine_core::command::Command;
use engine_core::price::SourceQuote;
use engine_core::types::MarketId;
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, sleep, timeout};

use crate::clock::now_ms;
use crate::config::{Venue, VenueConnection, VenueSymbol};
use crate::feeds::status::FeedStatus;
use crate::feeds::tests::fake::{
    Conn, FakeEngine, WAIT, accept, fake_sequencer, fast, kraken_book, listen, quote, send,
};
use crate::feeds::venue::{self, Credentials, Held, Timing};
use crate::sequencer::Request;
use crate::test_support::capture_alerts;

const BTC: [(MarketId, &str); 1] = [(1, "BTC/USD")];
/// The connection's read deadline and silence window, which the hold outlasts three times over.
const TIMEOUT: Duration = Duration::from_millis(400);
/// What a Kraken connection sends while its books stand.
const KRAKEN_HEARTBEAT: &str = r#"{"channel":"heartbeat"}"#;

/// Held back past its read deadline and silence window, a feed sends its sequencer nothing, neither a
/// quote nor a question, raises no alert and keeps its connection; once live it asks the engine first,
/// as a fresh connection does, then sends the newest book it read while held back, with no book read
/// since, and only that one.
#[tokio::test]
async fn a_held_back_feed_sends_nothing_until_live_and_then_its_newest_kept_book_at_once() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (tx, mut rx) = mpsc::channel(1024);
    let (live, held) = watch::channel(false);
    let _feed = tokio::spawn(venue::run_reporting(
        connection(&url),
        Credentials::Public,
        fast(TIMEOUT),
        venue::tls_config().expect("tls config"),
        (tx, Held { live: held, hours: Default::default() }),
        (FeedStatus::default(), 0),
    ));
    let (mut ws, _) = accept(&listener).await;
    // An older book and then the newest, both read while the feed holds back.
    let read_ts = now_ms();
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", read_ts - 5), kraken_book("BTC/USD", "200", "201", read_ts)])
        .await;
    beat(&mut ws, TIMEOUT * 3).await;
    assert!(rx.try_recv().is_err(), "a held-back feed sent its sequencer something");
    assert!(timeout(Duration::from_millis(10), listener.accept()).await.is_err(), "the held-back feed reconnected");
    assert!(alerts.try_recv().is_err(), "a held-back feed raised an alert");

    let flipped = Instant::now();
    live.send_replace(true);
    let mut engine = FakeEngine::new();
    match timeout(WAIT, rx.recv()).await.expect("a request in time").expect("feed running") {
        Request::Query(query) => engine.answer(query),
        _ => panic!("the feed's first request once live is not its question to the engine"),
    }
    let (kept, not_after) = next_quote(&mut rx, &mut engine).await.expect("the kept book once live");
    assert_eq!(kept, quote(Venue::Kraken, "200.5", read_ts), "not the newest book read while held back");
    assert!(not_after > std::time::Instant::now(), "the kept book's delay bound has run out");
    assert!(flipped.elapsed() < TIMEOUT, "the kept book waited for a later one: {:?}", flipped.elapsed());
    send(&mut ws, &[KRAKEN_HEARTBEAT.to_owned()]).await;
    let replaced = timeout(TIMEOUT / 2, next_quote(&mut rx, &mut engine)).await;
    assert!(replaced.is_err(), "a book replaced while held back was sent too: {replaced:?}");
}

/// A connection that drops while its feed holds back is no outage: it raises no alert. The backoff
/// its drops built up ends as soon as the switch is set, and starts afresh for the feed live: the
/// first live drop waits only `reconnect_min`.
#[tokio::test]
async fn a_held_back_connection_that_drops_raises_nothing_and_starts_afresh_once_live() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    // A sequencer that answers the feed's questions, so a live drop settles at once.
    let (tx, _rx) = fake_sequencer();
    let (live, held) = watch::channel(false);
    let reconnect_min = Duration::from_millis(100);
    let timing = Timing { reconnect_min, reconnect_max: Duration::from_secs(10), ..fast(WAIT) };
    let _feed = tokio::spawn(venue::run_reporting(
        connection(&url),
        Credentials::Public,
        timing,
        venue::tls_config().expect("tls config"),
        (tx, Held { live: held, hours: Default::default() }),
        (FeedStatus::default(), 0),
    ));
    // Four drops held back build its wait up to 1.6 s.
    for _ in 0..4 {
        drop(accept(&listener).await);
    }
    sleep(Duration::from_millis(100)).await;
    assert!(timeout(Duration::from_millis(10), listener.accept()).await.is_err(), "it reconnected inside its backoff");
    assert!(alerts.try_recv().is_err(), "a held-back connection's drop raised an alert");
    let flipped = Instant::now();
    live.send_replace(true);
    let (ws, _) = accept(&listener).await;
    assert!(
        flipped.elapsed() < Duration::from_millis(800),
        "it sat out its backoff once live: {:?}",
        flipped.elapsed()
    );
    let dropped = Instant::now();
    drop(ws);
    accept(&listener).await;
    assert!(dropped.elapsed() < Duration::from_millis(800), "its first live drop waited {:?}", dropped.elapsed());
}

/// A node that stops, as an incumbent does before it hands over its turn, closes its venue connections
/// at once, so a vendor that allows one live connection per account takes its successor's.
#[tokio::test]
async fn stopping_the_feeds_closes_their_connections_at_once() {
    let (listener, url) = listen().await;
    let (tx, _rx) = fake_sequencer();
    let mut feeds = tokio::task::JoinSet::new();
    feeds.spawn(async move {
        let _ = venue::run_reporting(
            connection(&url),
            Credentials::Public,
            fast(TIMEOUT),
            venue::tls_config().expect("tls config"),
            (tx, Held::never()),
            (FeedStatus::default(), 0),
        )
        .await;
    });
    let (mut ws, _) = accept(&listener).await;
    feeds.shutdown().await;
    let closed = timeout(WAIT, async {
        while let Some(Ok(message)) = ws.next().await {
            assert!(!message.is_text(), "the stopped feed sent {message:?}");
        }
    })
    .await;
    assert!(closed.is_ok(), "the connection stayed open after the feeds stopped");
}

/// Sends heartbeats for `span`, so the connection's read deadline never passes.
async fn beat(ws: &mut Conn, span: Duration) {
    let until = Instant::now() + span;
    while Instant::now() < until {
        send(ws, &[KRAKEN_HEARTBEAT.to_owned()]).await;
        sleep(Duration::from_millis(50)).await;
    }
}

/// The next quote the feed sends, with its delay bound, applied to `engine`, which answers the
/// feed's questions meanwhile; `None` once the feed has stopped.
async fn next_quote(
    rx: &mut mpsc::Receiver<Request>,
    engine: &mut FakeEngine,
) -> Option<(SourceQuote, std::time::Instant)> {
    loop {
        match timeout(WAIT, rx.recv()).await.expect("a request in time")? {
            Request::Quote { cmd: Command::Prices { market, quotes }, not_after, .. } => {
                let [quote] = quotes.as_slice() else { panic!("one quote a command: {quotes:?}") };
                let quote = quote.clone();
                engine.apply(Command::Prices { market, quotes });
                return Some((quote, not_after));
            }
            Request::Query(query) => engine.answer(query),
            _ => panic!("a feed sends quotes and questions only"),
        }
    }
}

/// A public Kraken connection to `url` for BTC/USD.
fn connection(url: &str) -> VenueConnection {
    let symbols =
        BTC.iter().map(|(market, symbol)| VenueSymbol { market: *market, symbol: (*symbol).into(), contract: None });
    VenueConnection {
        venue: Venue::Kraken,
        url: url.to_owned(),
        api_key_env: None,
        api_key_secret: None,
        oauth: None,
        key_pair: None,
        warm: None,
        symbols: symbols.collect(),
    }
}
