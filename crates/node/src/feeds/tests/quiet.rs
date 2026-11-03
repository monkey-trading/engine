//! A venue that repeats its top of book: a repeated mid is sequenced only to keep the engine's
//! quote fresh, a moved mid goes at once, and a new connection or a late quote is never held.

use engine_core::command::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::*;
use crate::feeds::tests::fake::{FakeEngine, spawn_feed_into};
use crate::sequencer::Request;

/// A `stale_after` of 400ms, so a repeat is refreshed after 200ms (half of it, the shorter of it
/// and the dev markets' 1s `max_age_ms`).
fn timing() -> Timing {
    Timing { stale_after: Duration::from_millis(400), ..fast(WAIT) }
}

#[tokio::test]
async fn a_repeated_mid_is_sequenced_only_to_keep_the_engines_quote_fresh() {
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], timing());
    let (ws, _) = accept(&listener).await;

    // The same book every 20ms, stamped as it is sent, for 1.2s: some sixty updates.
    let _serving = stream(ws, || vec![kraken_book("BTC/USD", "100", "101", now_ms())]);
    let mut quotes = vec![next_prices(&mut rx).await.1.remove(0)];
    let mut ages = Vec::new();
    let end = Instant::now() + Duration::from_millis(1_200);
    while let Ok((_, mut next)) = timeout(end.saturating_duration_since(Instant::now()), next_prices(&mut rx)).await {
        let next = next.remove(0);
        // How old the quote the engine held was when the refresh reached the sequencer.
        ages.push(now_ms() - quotes.last().unwrap().ts);
        quotes.push(next);
    }

    // Some sixty updates became the first quote and a refresh about every 200ms.
    assert!((4..=9).contains(&quotes.len()), "{} commands", quotes.len());
    assert!(quotes.iter().all(|quote| quote.mid.to_string() == "100.5"));
    // The engine's quote never reached the 400ms after which it stops counting.
    assert!(ages.iter().all(|age| (150..400).contains(age)), "{ages:?}");
}

#[tokio::test]
async fn a_mid_that_moves_is_sequenced_at_once_among_repeats() {
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], timing());
    let (mut ws, _) = accept(&listener).await;

    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
    assert_eq!(next_prices(&mut rx).await.1[0].mid.to_string(), "100.5");
    // Two repeats inside the refresh time sequence nothing.
    for _ in 0..2 {
        sleep(Duration::from_millis(20)).await;
        send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
    }
    assert!(timeout(Duration::from_millis(60), next_prices(&mut rx)).await.is_err(), "a repeat was sequenced");
    // A move goes at once, long before the refresh is due, and so does its move back.
    for mid in ["102.5", "100.5"] {
        let (bid, ask) = if mid == "102.5" { ("102", "103") } else { ("100", "101") };
        send(&mut ws, &[kraken_book("BTC/USD", bid, ask, now_ms())]).await;
        let (_, quotes) = timeout(Duration::from_millis(100), next_prices(&mut rx)).await.expect("sequenced at once");
        assert_eq!(quotes[0].mid.to_string(), mid);
    }
}

#[tokio::test]
async fn a_repeat_whose_predecessor_is_already_old_enough_is_sequenced_at_once() {
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], timing());
    let (mut ws, _) = accept(&listener).await;

    // A quote that reaches the host already 250ms old, past the 200ms refresh time: the repeat
    // that follows has no time to wait, as a venue running late would send.
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms() - 250)]).await;
    next_prices(&mut rx).await;
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
    timeout(Duration::from_millis(100), next_prices(&mut rx)).await.expect("sequenced at once");
}

#[tokio::test]
async fn a_new_connection_sequences_its_first_quote_though_the_mid_is_unchanged() {
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &[(1, "BTC/USD")], timing());

    let (mut first, _) = accept(&listener).await;
    send(&mut first, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
    next_prices(&mut rx).await;
    drop(first);

    let (mut second, _) = accept(&listener).await;
    send(&mut second, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
    let (_, quotes) = timeout(Duration::from_millis(100), next_prices(&mut rx)).await.expect("sequenced at once");
    assert_eq!(quotes[0].mid.to_string(), "100.5");
}

#[tokio::test]
async fn a_repeat_waiting_on_a_dropped_quote_goes_when_the_engine_is_found_not_to_hold_it() {
    let (listener, url) = listen().await;
    let (feed_tx, mut feed_rx) = mpsc::channel(64);
    let (seen_tx, mut seen) = mpsc::channel(64);
    // A sequencer that drops the second command it is sent, as one does while trusted time
    // refuses its stamp, and answers the feed's questions from the engine.
    tokio::spawn(async move {
        let mut engine = FakeEngine::new();
        let mut submitted = 0;
        while let Some(request) = feed_rx.recv().await {
            match request {
                Request::Query(query) => engine.answer(query),
                Request::Quote { cmd, .. } => {
                    submitted += 1;
                    if submitted != 2 {
                        engine.apply(cmd.clone());
                    }
                    let _ = seen_tx.send(cmd).await;
                }
                _ => unreachable!("a venue feed sends only commands and questions"),
            }
        }
    });
    let _feed = spawn_feed_into(feed_tx, Venue::Kraken, &url, &[(1, "BTC/USD")], fast(WAIT));
    let (mut ws, _) = accept(&listener).await;

    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
    seen.recv().await.expect("the first quote");
    // The move is dropped, and a repeat of it follows at once: it waits, since the feed believes
    // the engine holds the move. The next confirmation finds it does not, and the repeat goes then,
    // not 500ms on, when the refresh would have fallen due.
    send(&mut ws, &[kraken_book("BTC/USD", "102", "103", now_ms() - 5)]).await;
    seen.recv().await.expect("the dropped move");
    send(&mut ws, &[kraken_book("BTC/USD", "102", "103", now_ms())]).await;
    let Command::Prices { quotes, .. } =
        timeout(Duration::from_millis(300), seen.recv()).await.expect("sequenced at once").expect("a command")
    else {
        panic!("a price command")
    };
    assert_eq!(quotes[0].mid.to_string(), "102.5");
}
