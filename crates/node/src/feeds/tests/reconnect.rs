//! When a feed reconnects: at once after a connection that sent quotes drops, and after
//! `reconnect_min` once the attempt after that fails before it sends one, though the engine
//! still holds the dropped connection's quote.

use std::time::Duration;

use tokio::time::{Instant, sleep};

use crate::clock::now_ms;
use crate::config::Venue;
use crate::feeds::tests::fake::{WAIT, accept, fast, kraken_book, listen, next_prices, quote, send, spawn_feed};
use crate::feeds::venue::Timing;

#[tokio::test]
async fn a_healthy_connection_reconnects_at_once_and_a_second_failure_in_a_row_waits() {
    let timing = Timing { reconnect_min: Duration::from_secs(1), reconnect_max: Duration::from_secs(2), ..fast(WAIT) };
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], timing);

    // A connection that quoted drops, as a venue's reset does: the feed connects again at once.
    let (mut first, _) = accept(&listener).await;
    let ts = now_ms();
    send(&mut first, &[kraken_book("BTC/USD", "100", "101", ts)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "100.5", ts)]));
    // Past `reconnect_min` from its attempt, which spaces attempts however briefly one lives, and
    // with a quote fresh as it drops, which the engine still holds while the next connection opens.
    sleep(timing.reconnect_min).await;
    let ts = now_ms();
    send(&mut first, &[kraken_book("BTC/USD", "100", "102", ts)]).await;
    assert_eq!(next_prices(&mut rx).await, (1, vec![quote(Venue::Kraken, "101", ts)]));
    drop(first);
    let dropped = Instant::now();
    let (second, _) = accept(&listener).await;
    assert!(dropped.elapsed() < timing.reconnect_min, "reconnected {:?} after the drop", dropped.elapsed());

    // That connection sends no quote, though the engine held a fresh one from the venue as it
    // opened, and drops past `reconnect_min`: the second failure in a row, so the next attempt
    // waits the backoff.
    sleep(timing.reconnect_min).await;
    drop(second);
    let dropped = Instant::now();
    let _third = accept(&listener).await;
    assert!(dropped.elapsed() >= timing.reconnect_min, "reconnected {:?} after the failure", dropped.elapsed());
}
