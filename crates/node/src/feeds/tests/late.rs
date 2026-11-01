//! Venue tasks against a sequencer that answers late: a quote counts as the venue quoting by when
//! the sequencer has applied it, and a connection's last quotes are settled after it ends.

use std::time::Duration;

use engine_core::command::Command;
use engine_core::types::MarketId;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

use crate::clock::now_ms;
use crate::config::Venue;
use crate::feeds::tests::fake::{
    FakeEngine, WAIT, accept, fast, kraken_book, listen, next_alert, send, spawn_feed_into, stream,
};
use crate::feeds::venue::Timing;
use crate::sequencer::Request;
use crate::test_support::capture_alerts;

#[tokio::test]
async fn quotes_the_sequencer_applies_too_late_do_not_count_as_the_venue_quoting() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    // A sequencer that has fallen behind: it reaches each query, and so has applied the quotes
    // sent before it, only after the feed's freshness bound, though well within the silence window.
    let timing = Timing { stale_after: Duration::from_millis(200), ..fast(Duration::from_secs(2)) };
    let (tx, _arrivals) = late_sequencer(timing.stale_after * 2);
    let _feed = spawn_feed_into(tx, Venue::Kraken, &url, &[(1, "BTC/USD")], timing);
    let (ws, _) = accept(&listener).await;

    stream(ws, || vec![kraken_book("BTC/USD", "100", "101", now_ms())]);
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["symbol"].as_str()), ("venue_symbol_silent", "BTC/USD"));
}

#[tokio::test]
async fn a_connection_that_ends_before_its_quote_is_confirmed_still_counts_as_quoting() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (tx, mut arrivals) = late_sequencer(Duration::from_millis(300));
    let _feed = spawn_feed_into(tx, Venue::Kraken, &url, &[(1, "BTC/USD")], fast(WAIT));
    let (mut ws, _) = accept(&listener).await;

    // The venue quotes once and drops the connection as soon as the feed asks for its
    // confirmation, behind the market hours the feed asks for when it connects.
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
    wait_for(&mut arrivals, Arrival::Prices(1)).await;
    wait_for(&mut arrivals, Arrival::Query).await;
    drop(ws);

    // The connection quoted, so its end is a routine drop: the feed reconnects with no outage alert.
    let (_next, _) = accept(&listener).await;
    assert!(alerts.try_recv().is_err(), "the drop was alerted as an outage");
}

#[tokio::test]
async fn a_quote_sequenced_while_a_confirmation_is_awaited_is_settled_when_the_connection_ends() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let (tx, mut arrivals) = late_sequencer(Duration::from_millis(300));
    let _feed = spawn_feed_into(tx, Venue::Kraken, &url, &[(1, "BTC/USD"), (2, "ETH/USD")], fast(WAIT));
    let (mut ws, _) = accept(&listener).await;

    // A stale BTC quote is confirmed first, behind the market hours the feed asks for when it
    // connects, and while that answer is awaited a fresh ETH quote is sequenced; the connection
    // then drops, so only the last confirmation can count ETH.
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms() - 60_000)]).await;
    wait_for(&mut arrivals, Arrival::Prices(1)).await;
    wait_for(&mut arrivals, Arrival::Query).await;
    send(&mut ws, &[kraken_book("ETH/USD", "10", "11", now_ms())]).await;
    wait_for(&mut arrivals, Arrival::Prices(2)).await;
    drop(ws);

    let (_next, _) = accept(&listener).await;
    assert!(alerts.try_recv().is_err(), "the drop was alerted as an outage");
}

// ── Helpers ──

/// What reached a late sequencer.
#[derive(Debug, PartialEq)]
enum Arrival {
    Prices(MarketId),
    Query,
}

/// A sequencer that applies what the feed sequences to a `FakeEngine` at once but answers each
/// query only `delay` after it arrives, reporting every arrival on the returned receiver.
fn late_sequencer(delay: Duration) -> (mpsc::Sender<Request>, mpsc::UnboundedReceiver<Arrival>) {
    let (tx, mut requests) = mpsc::channel(1024);
    let (arrived, arrivals) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut engine = FakeEngine::new();
        while let Some(request) = requests.recv().await {
            match request {
                Request::Submit { cmd, .. } => {
                    if let Command::Prices { market, .. } = &cmd {
                        let _ = arrived.send(Arrival::Prices(*market));
                    }
                    engine.apply(cmd);
                }
                Request::Query(query) => {
                    let _ = arrived.send(Arrival::Query);
                    sleep(delay).await;
                    engine.answer(query);
                }
                Request::CheckpointStatus { .. } => unreachable!("no checkpoint worker in this test"),
                Request::WatchBenchmarkSchedule(_) => unreachable!("only the API watches benchmark schedules"),
                Request::ReissueWithdrawal { .. } => unreachable!("only the API asks for a reissue"),
                Request::AccountPayouts { .. } => unreachable!("only the API asks for payouts"),
                Request::FinalCheckpoint(_) => unreachable!("only a handover asks for the final checkpoint"),
                Request::TimeSample(_) => unreachable!("a feed sends no time samples"),
                Request::Sync(_) => unreachable!("only custody asks for a log sync"),
            }
        }
    });
    (tx, arrivals)
}

/// Waits until `expected` reaches the late sequencer, failing the test after `WAIT`.
async fn wait_for(arrivals: &mut mpsc::UnboundedReceiver<Arrival>, expected: Arrival) {
    let reached = async { while arrivals.recv().await.expect("sequencer running") != expected {} };
    timeout(WAIT, reached).await.expect("arrived in time");
}
