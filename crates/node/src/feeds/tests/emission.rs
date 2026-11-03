//! The emission path: one sequenced quote per book update as it arrives, whatever its stamp, the
//! per-symbol budget that coalesces what arrives faster than it refills, and a venue clock ahead of
//! the host's.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engine_core::Fixed;

use super::*;
use crate::test_support::capture_alerts;

#[tokio::test]
async fn each_book_update_sequences_its_symbols_quote_as_it_arrives() {
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &MARKETS, fast(WAIT));
    let (ws, _) = accept(&listener).await;

    // One update every 20ms, twice the 10ms pace, so three updates inside one former 250ms
    // interval each sequence as they arrive: nothing collapses to the newest. (That the first
    // update of a burst sequences at once, which interval batching cannot do, is asserted by
    // `updates_faster_than_the_pace_coalesce_to_the_newest`.)
    let step = Arc::new(AtomicUsize::new(0));
    let books = {
        let step = Arc::clone(&step);
        move || {
            let i = step.fetch_add(1, Ordering::Relaxed) as i64;
            vec![kraken_book("BTC/USD", &format!("{}", 100 + i), &format!("{}", 101 + i), now_ms())]
        }
    };
    let _serving = stream(ws, books);
    for i in 0..3 {
        let within = timeout(Duration::from_secs(2), next_prices(&mut rx)).await.expect("a quote per update");
        let (market, quotes) = within;
        let mid = "100.5".parse::<Fixed>().unwrap() + Fixed::from_int(i);
        assert_eq!((market, quotes.len(), quotes[0].mid), (1, 1, mid));
    }
}

#[tokio::test]
async fn updates_past_the_budget_coalesce_to_the_newest() {
    let (listener, url) = listen().await;
    // A wide spacing, so the updates of one read all land inside the budget's refill time.
    let timing = Timing { interval: Duration::from_millis(200), ..fast(WAIT) };
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], timing);
    let (mut ws, _) = accept(&listener).await;

    // Twelve moves at once. The burst of eight goes as it arrives, each its own quote; the other
    // four coalesce, and the newest is the next quote through once the budget has refilled.
    let books: Vec<_> =
        (0..12).map(|i| kraken_book("BTC/USD", &format!("{}", 100 + i), &format!("{}", 101 + i), now_ms())).collect();
    send(&mut ws, &books).await;
    let sent = now_ms();
    for i in 0..8 {
        let (market, quotes) = next_prices(&mut rx).await;
        assert_eq!((market, quotes[0].mid.to_string()), (1, format!("{}.5", 100 + i)));
    }
    assert!(now_ms() - sent < 150, "the burst waited on a timer");
    let (market, newest) = next_prices(&mut rx).await;
    assert_eq!((market, newest[0].mid.to_string()), (1, "111.5".into()));
    let more = timeout(Duration::from_millis(50), next_prices(&mut rx)).await;
    assert!(more.is_err(), "a coalesced update sequenced past the budget");
}

#[tokio::test]
async fn a_quote_stamped_ahead_of_the_host_clock_is_sequenced_as_it_is_read() {
    let (listener, url) = listen().await;
    let window = Duration::from_millis(500);
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], fast(window));
    let (mut ws, _) = accept(&listener).await;

    // A book, then one dated 800ms ahead. Neither waits for the clock.
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
    let (_, first) = next_prices(&mut rx).await;
    assert_eq!(first[0].mid.to_string(), "100.5");
    send(&mut ws, &[kraken_book("BTC/USD", "102", "103", now_ms() + 800)]).await;
    let (_, second) = timeout(Duration::from_millis(500), next_prices(&mut rx)).await.expect("not held for the clock");
    assert_eq!(second[0].mid.to_string(), "102.5");
    assert!(second[0].ts > first[0].ts + 500, "the stamp is the venue's, not rewritten by the feed");
}

#[tokio::test]
async fn a_venue_clock_ahead_of_the_hosts_counts_as_quoting_with_no_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let window = Duration::from_millis(500);
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], fast(window));
    let (ws, _) = accept(&listener).await;

    // Each update is dated 800ms ahead of the host clock, as a venue clock that leads would date
    // it. The engine stores it as of the time it was sequenced, so it ages from then: the venue
    // keeps counting as quoting through several silence windows.
    stream(ws, || vec![kraken_book("BTC/USD", "100", "101", now_ms() + 800)]);
    let (_, quotes) = next_prices(&mut rx).await;
    assert!(quotes[0].ts > now_ms(), "held for the clock instead of sequenced at once");
    sleep(window * 3).await;
    if let Ok(alert) = alerts.try_recv() {
        panic!("the venue was alerted: {alert:?}");
    }
}
