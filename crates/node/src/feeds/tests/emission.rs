//! The emission path: one sequenced quote per book update as it arrives, the per-symbol pace that
//! coalesces what arrives faster, and the emit timer that releases what the host clock has not
//! reached yet — with the venue quiet.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engine_core::Fixed;

use super::*;
use crate::feeds::tests::fake::kraken_books;

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
async fn updates_faster_than_the_pace_coalesce_to_the_newest() {
    let (listener, url) = listen().await;
    // A wide pace, so the five updates of one read all land inside the first quote's window.
    let timing = Timing { interval: Duration::from_millis(200), ..fast(WAIT) };
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], timing);
    let (mut ws, _) = accept(&listener).await;

    // The first update sequences at once — which interval batching cannot do, it would sequence
    // only the newest of the burst; the rest arrive before the pace frees the symbol, so they
    // coalesce, and the newest is the next quote through.
    let books: Vec<_> =
        (0..5).map(|i| kraken_book("BTC/USD", &format!("{}", 100 + i), &format!("{}", 101 + i), now_ms())).collect();
    send(&mut ws, &books).await;

    let (market, first) = next_prices(&mut rx).await;
    assert_eq!((market, first[0].mid.to_string()), (1, "100.5".into()));
    // The pace holds the symbol until the window ends, then lets the newest coalesced update
    // through; nothing else of the burst sequences.
    let (market, newest) = next_prices(&mut rx).await;
    assert_eq!((market, newest[0].mid.to_string()), (1, "104.5".into()));
    let more = timeout(Duration::from_millis(50), next_prices(&mut rx)).await;
    assert!(more.is_err(), "a coalesced update sequenced past the pace");
}

#[tokio::test]
async fn a_quote_waiting_on_the_clock_or_the_pace_sequences_without_more_traffic() {
    let (listener, url) = listen().await;
    // A wide pace, so the second quote waits it out once the host clock has reached its stamp.
    let timing = Timing { interval: Duration::from_millis(100), ..fast(WAIT) };
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], timing);
    let (mut ws, _) = accept(&listener).await;

    // One message carries both books. The first is reached and sequences at once; the second is
    // dated a little ahead and waits for the clock and then the pace: the send must arm the feed's
    // own timer for it, or with the venue quiet it would strand. Interval batching sequences only
    // the newest of the message, and an emitter that does not re-arm after a send strands it, so
    // both fail this test.
    send(&mut ws, &[kraken_books("BTC/USD", &[("100", "101", now_ms()), ("102", "103", now_ms() + 30)])]).await;
    let (market, first) = next_prices(&mut rx).await;
    assert_eq!((market, first[0].mid.to_string()), (1, "100.5".into()));
    let (market, second) = next_prices(&mut rx).await;
    assert_eq!((market, second[0].mid.to_string()), (1, "102.5".into()));
    assert!(second[0].ts <= now_ms(), "sent before the host clock reached its stamp");
}

#[tokio::test]
async fn quotes_a_little_ahead_of_the_host_clock_are_sequenced_once_it_reaches_them() {
    let (_guard, mut alerts) = capture_alerts();
    let (listener, url) = listen().await;
    let window = Duration::from_millis(500);
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], fast(window));
    let (ws, _) = accept(&listener).await;

    // Each update is dated a little ahead of the host clock, so emission holds it until the
    // clock reaches it, then sequences it: the stamp never claims a time the clock has not
    // reached. Each goes once reached, and the venue counts as quoting.
    stream(ws, || vec![kraken_book("BTC/USD", "100", "101", now_ms() + 100)]);
    let (_, quotes) = next_prices(&mut rx).await;
    assert!(quotes[0].ts <= now_ms(), "sequenced before the host clock reached it");
    sleep(window * 3).await;
    assert!(alerts.try_recv().is_err(), "the venue was alerted");
}
