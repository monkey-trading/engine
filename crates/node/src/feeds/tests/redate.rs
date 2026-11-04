//! A quiet venue's book re-dated by its pings, against loopback venues that send a book and then
//! only answer pings: what is sequenced, how it is stamped, when it stops, and, through the real
//! sequencer, that the market stays priced until the venue stops answering.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat};
use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::price::{PriceError, SourceQuote};
use engine_core::types::{MarketId, Millis};
use futures_util::SinkExt;
use futures_util::StreamExt;
use futures_util::stream::SplitSink;
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;

use crate::clock::now_ms;
use crate::config::Venue;
use crate::feeds::kraken;
use crate::feeds::tests::fake::{
    Conn, FakeEngine, WAIT, accept, bitstamp_book, coinbase_levels, kraken_book, listen, next_prices, quote, send,
    spawn_feed, spawn_feed_into, stream,
};
use crate::feeds::tests::rwa_support::around_the_clock;
use crate::feeds::tests::scenario::Node;
use crate::feeds::venue::Timing;
use crate::sequencer::Request;
use crate::test_support::dev_config;

const BTC: [(MarketId, &str); 1] = [(1, "BTC/USD")];
/// Pings often enough to re-date a quiet book many times over its market's `max_age_ms`.
const TIMING: Timing = Timing {
    interval: Duration::from_millis(10),
    timeout: WAIT,
    reconnect_min: Duration::from_millis(10),
    reconnect_max: Duration::from_millis(40),
    stale_after: Duration::from_secs(1),
    ping_interval: Duration::from_millis(50),
    max_delay: Duration::from_secs(2),
};
/// The longest a book stays re-dated, as `standing::MAX_BOOK_SILENCE` sets it.
const MAX_BOOK_SILENCE_MS: Millis = 5_000;
/// What a Kraken connection sends while its books stand.
const KRAKEN_HEARTBEAT: &str = r#"{"channel":"heartbeat"}"#;

#[tokio::test]
async fn a_quiet_book_is_sequenced_again_at_later_stamps_each_bounded_by_a_ping_sent_after_its_read() {
    let mut engine = FakeEngine::new();
    let (listener, url) = listen().await;
    let (tx, mut rx) = mpsc::channel(1024);
    let _feed = spawn_feed_into(tx, Venue::Kraken, &url, &BTC, TIMING);
    let (mut ws, _) = accept(&listener).await;
    let read_ts = now_ms();
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", read_ts)]).await;
    let (first, read_bound) = next_quote(&mut rx, &mut engine).await;
    assert_eq!(first, quote(Venue::Kraken, "100.5", read_ts));
    // The bound of the ping answered before the book was read.
    let read_bound = read_bound.expect("a connection with pings");

    // The venue sends nothing more but answers each ping as it arrives, noting when the newest did.
    let venue = answer_pings(ws, Duration::ZERO);
    let mut last = read_ts;
    while last < read_ts + 1_500 {
        let (redated, bound) = next_quote(&mut rx, &mut engine).await;
        assert_eq!(redated.mid, "100.5".parse().unwrap());
        assert!(redated.ts > last, "each stamp is later than the one before");
        assert!(redated.ts <= venue.pinged.load(Ordering::SeqCst), "stamped after the newest ping reached the venue");
        // Bounded by the ping whose answer re-dated it, sent after the book was read.
        let bound = bound.expect("a connection with pings");
        assert!(bound > read_bound, "a re-dated quote kept the bound of the ping answered before the read");
        assert!(bound <= std::time::Instant::now() + TIMING.max_delay, "bounded by a ping not yet sent");
        last = redated.ts;
    }
}

#[tokio::test]
async fn an_answer_the_host_delays_only_ages_the_quote_it_redates() {
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &BTC, TIMING);
    let (mut ws, _) = accept(&listener).await;
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
    next_prices(&mut rx).await;

    // Each answer leaves `held` after its ping arrived, so the quote it re-dates is at least that
    // old when it is sequenced.
    let held = Duration::from_millis(300);
    let _venue = answer_pings(ws, held);
    for _ in 0..3 {
        let (_, quotes) = next_prices(&mut rx).await;
        let age = now_ms() - quotes[0].ts;
        assert!(age >= held.as_millis() as Millis, "a quote {age}ms old came through an answer held {held:?}");
    }
}

#[tokio::test]
async fn a_book_is_redated_for_max_book_silence_and_no_longer() {
    let (listener, url) = listen().await;
    let timing = Timing { timeout: WAIT * 2, ..TIMING };
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &BTC, timing);
    let (mut ws, _) = accept(&listener).await;
    let read_ts = now_ms();
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", read_ts)]).await;
    next_prices(&mut rx).await;

    // Heartbeats keep the connection's reads alive, and every ping is answered.
    let venue = stream(ws, || vec![KRAKEN_HEARTBEAT.to_owned()]);
    let mut last = read_ts;
    // A second with nothing sequenced ends the wait, which fails rather than hangs if the re-dating
    // never stops.
    let quiet = async {
        while let Ok(Some(request)) = timeout(Duration::from_secs(1), rx.recv()).await {
            if let Request::Submit { cmd: Command::Prices { quotes, .. }, .. } = request {
                last = quotes[0].ts;
            }
        }
    };
    let bound = Duration::from_millis(MAX_BOOK_SILENCE_MS as u64) + WAIT;
    timeout(bound, quiet).await.expect("re-dated past the bound");
    // The last stamp is within a ping, and a busy runner's stall, of the bound, and never past it.
    let redated_for = last - read_ts;
    assert!(redated_for <= MAX_BOOK_SILENCE_MS, "re-dated {redated_for}ms past the book");
    assert!(redated_for >= MAX_BOOK_SILENCE_MS - 1_000, "re-dated only {redated_for}ms past the book");
    venue.abort();
}

#[tokio::test]
async fn a_venue_that_stops_answering_pings_is_redated_no_more() {
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &BTC, TIMING);
    let (mut ws, _) = accept(&listener).await;
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
    next_prices(&mut rx).await;
    let venue = answer_pings(ws, Duration::ZERO);
    next_prices(&mut rx).await;
    next_prices(&mut rx).await;

    // The venue stays connected but reads nothing more, so it answers no ping from now on.
    let _silent = venue.stop().await;
    // A repeat the last answer deferred can still fire within the refresh margin; after it, only
    // a new answer could re-date the book, and none comes.
    let quiet = async { while timeout(Duration::from_secs(1), rx.recv()).await.is_ok() {} };
    timeout(WAIT * 2, quiet).await.expect("a quote no answer vouched for");
}

#[tokio::test]
async fn a_book_read_after_a_redated_one_replaces_it_whatever_its_stamp() {
    let mut engine = FakeEngine::new();
    let (listener, url) = listen().await;
    let (mut rx, _feed) = spawn_feed(Venue::Kraken, &url, &BTC, TIMING);
    let (mut ws, _) = accept(&listener).await;
    let read_ts = now_ms();
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", read_ts)]).await;
    let mut apply = |(market, quotes): (MarketId, Vec<SourceQuote>)| engine.apply(Command::Prices { market, quotes });
    apply(next_prices(&mut rx).await);

    // Re-dated past the stamp of the venue's next book, as when a venue answers a ping ahead of a
    // change it made just before.
    let change = Arc::new(AtomicBool::new(false));
    let book = kraken_book("BTC/USD", "200", "201", read_ts + 100);
    let changed = change.clone();
    let venue = stream(ws, move || if changed.swap(false, Ordering::SeqCst) { vec![book.clone()] } else { vec![] });
    let redated = loop {
        let (market, quotes) = next_prices(&mut rx).await;
        let ts = quotes[0].ts;
        apply((market, quotes));
        if ts >= read_ts + 300 {
            break ts;
        }
    };
    change.store(true, Ordering::SeqCst);
    let replaced = loop {
        let (market, quotes) = next_prices(&mut rx).await;
        let quote = quotes[0].clone();
        apply((market, quotes));
        if quote.mid == "200.5".parse().unwrap() {
            break quote;
        }
    };
    assert!(replaced.ts >= redated, "the book is stamped {} before the re-dated {redated}", replaced.ts);
    let (reply, held) = oneshot::channel();
    engine.answer(Box::new(move |engine| {
        let _ = reply.send(engine.markets()[&1].quotes["kraken"].mid);
    }));
    assert_eq!(held.await.expect("an answer"), "200.5".parse().unwrap(), "the engine holds the venue's new mid");
    venue.abort();
}

#[tokio::test]
async fn a_refused_message_stops_the_redating_until_the_next_book() {
    // Bitstamp keeps no book, so a refused message leaves the connection up: one that does not
    // parse, and a book that does not check. The venue answers pings throughout, so a held book
    // re-dates until the refusal, and the next book is held again.
    for refused in ["not json".to_owned(), bitstamp_book("btcusd", "101", "100", now_ms())] {
        let (listener, url) = listen().await;
        let (mut rx, _feed) = spawn_feed(Venue::Bitstamp, &url, &[(1, "btcusd")], TIMING);
        let (ws, _) = accept(&listener).await;
        let venue = answer_pings(ws, Duration::ZERO);
        // A book read once a ping's answer is on its way is held and re-dates.
        timeout(WAIT, async {
            while venue.pinged.load(Ordering::SeqCst) == 0 {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a ping answered in time");
        venue.send(&[bitstamp_book("btcusd", "100", "101", now_ms())]).await;
        assert_eq!(next_prices(&mut rx).await.1[0].mid, "100.5".parse().unwrap());
        // The venue sends nothing more, so only the re-dating sequences again.
        assert_eq!(next_prices(&mut rx).await.1[0].mid, "100.5".parse().unwrap());

        venue.send(std::slice::from_ref(&refused)).await;
        // The refusal forgot the book, and a repeat it deferred can fire only within the refresh
        // margin, so a second without a quote shows the re-dating stopped.
        let quiet = async { while timeout(Duration::from_secs(1), rx.recv()).await.is_ok() {} };
        timeout(WAIT * 2, quiet).await.expect("a quote after the refusal");

        // The next book is held again and re-dates.
        venue.send(&[bitstamp_book("btcusd", "100", "101", now_ms())]).await;
        assert_eq!(next_prices(&mut rx).await.1[0].mid, "100.5".parse().unwrap());
        assert_eq!(next_prices(&mut rx).await.1[0].mid, "100.5".parse().unwrap());
    }
}

#[tokio::test]
async fn a_changed_book_read_while_the_stream_is_behind_stops_the_redating() {
    let (listener, url) = listen().await;
    let timing = Timing { max_delay: Duration::from_millis(300), ..TIMING };
    // Bitstamp keeps no book, so only the feed knows that a later book changed the one it holds.
    let (mut rx, _feed) = spawn_feed(Venue::Bitstamp, &url, &[(1, "btcusd")], timing);
    let (mut ws, _) = accept(&listener).await;
    send(&mut ws, &[bitstamp_book("btcusd", "100", "101", now_ms())]).await;
    next_prices(&mut rx).await;

    // The venue reads nothing for longer than the delay bound, so the changed book it sends then is
    // read while the stream may be behind, and dropped. It then answers pings again.
    sleep(timing.max_delay * 2).await;
    send(&mut ws, &[bitstamp_book("btcusd", "200", "201", now_ms())]).await;
    let _venue = answer_pings(ws, Duration::ZERO);
    let quiet = timeout(Duration::from_millis(500), rx.recv()).await;
    assert!(quiet.is_err(), "re-dated a book a later one, dropped, had changed");
}

#[tokio::test]
async fn a_kept_book_left_without_a_top_is_not_redated() {
    let ts = now_ms();
    let kraken_emptied = {
        let at =
            DateTime::from_timestamp_millis(ts + 1).expect("in range").to_rfc3339_opts(SecondsFormat::Micros, true);
        let checksum = kraken::checksum([("101", "2.50000000")].into_iter());
        format!(
            r#"{{"channel":"book","type":"update","data":[{{"symbol":"BTC/USD","bids":[{{"price":100,"qty":0}}],"asks":[],"checksum":{checksum},"timestamp":"{at}"}}]}}"#
        )
    };
    let coinbase_emptied = coinbase_levels(2, false, "BTC-USD", "100", "101", ts + 1).replacen(
        r#""new_quantity":"1.25""#,
        r#""new_quantity":"0""#,
        1,
    );
    let cases = [
        (Venue::Kraken, "BTC/USD", kraken_book("BTC/USD", "100", "101", ts), kraken_emptied),
        (Venue::Coinbase, "BTC-USD", coinbase_levels(1, true, "BTC-USD", "100", "101", ts), coinbase_emptied),
    ];
    // Each update empties the bid side: the book has no top, so it yields no book.
    for (venue, symbol, book, emptied) in cases {
        let (listener, url) = listen().await;
        let (mut rx, _feed) = spawn_feed(venue, &url, &[(1, symbol)], TIMING);
        let (mut ws, _) = accept(&listener).await;
        send(&mut ws, &[book]).await;
        next_prices(&mut rx).await;
        send(&mut ws, &[emptied]).await;
        let _venue = answer_pings(ws, Duration::ZERO);
        let quiet = timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(quiet.is_err(), "{venue:?} re-dated a book whose top had gone");
    }
}

#[tokio::test]
async fn a_quiet_venue_keeps_the_market_priced_until_it_stops_answering_pings() {
    let node = Node::boot("quiet-venue", &dev_config().genesis).await;
    let timing = Timing { timeout: WAIT * 2, ..TIMING };
    let max_age = Duration::from_millis(1_000);
    let mid: Fixed = "100.5".parse().unwrap();

    // Bitstamp streams books throughout; Kraken sends one, then answers every ping and sends
    // nothing else, until it stops reading.
    let (bitstamp, bitstamp_url) = listen().await;
    spawn_feed_into(node.tx.clone(), Venue::Bitstamp, &bitstamp_url, &[(1, "btcusd")], timing);
    let (ws, _) = accept(&bitstamp).await;
    let bitstamp_books = stream(ws, || vec![bitstamp_book("btcusd", "100", "101", now_ms())]);
    let (kraken, kraken_url) = listen().await;
    spawn_feed_into(node.tx.clone(), Venue::Kraken, &kraken_url, &BTC, timing);
    let (mut ws, _) = accept(&kraken).await;
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
    let kraken_quiet = answer_pings(ws, Duration::ZERO);
    timeout(WAIT, async {
        while node.index(1).await.is_err() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("priced from both venues");

    // Priced at every look for three times the market's `max_age_ms`.
    let quiet_since = Instant::now();
    while quiet_since.elapsed() < max_age * 3 {
        assert_eq!(node.index(1).await, Ok(mid), "after {:?} of Kraken quiet", quiet_since.elapsed());
        sleep(Duration::from_millis(50)).await;
    }

    // Kraken stops answering, and its quote stops counting within the bound on a quote's age.
    let _ws = kraken_quiet.stop().await;
    let stopped = Instant::now();
    let unpriced = async {
        while node.index(1).await != Err(PriceError::Stale { fresh: 1, needed: 2 }) {
            sleep(Duration::from_millis(10)).await;
        }
    };
    timeout(WAIT, unpriced).await.expect("Kraken stops counting");
    let after = stopped.elapsed();
    assert!(after <= max_age + timing.max_delay, "Kraken counted for {after:?} after it stopped answering");
    bitstamp_books.abort();
}

#[tokio::test]
async fn a_book_read_before_its_market_closes_is_not_redated_past_the_closure() {
    const AAPL: MarketId = 40;
    // A one-second closure, a little after the venue's book is read.
    let start = now_ms();
    let closure = (start + 2_500, start + 3_500);
    let mut genesis = dev_config().genesis;
    genesis.markets.iter_mut().filter(|market| market.id == AAPL).for_each(|market| {
        market.schedule = around_the_clock(Some(closure));
    });
    let node = Node::boot("closing-quiet", &genesis).await;
    let applied = node.record();
    let (listener, url) = listen().await;
    spawn_feed_into(node.tx.clone(), Venue::Kraken, &url, &[(AAPL, "AAPL")], Timing { timeout: WAIT * 2, ..TIMING });
    let (mut ws, _) = accept(&listener).await;
    let read_ts = now_ms();
    assert!(read_ts < closure.0 - 1_000, "set up too slowly for the closure to come after the book");
    send(&mut ws, &[kraken_book("AAPL", "100", "101", read_ts)]).await;
    let _venue = answer_pings(ws, Duration::ZERO);

    // Re-dated up to the closure, and not after it, though the market reopens and every ping is
    // answered.
    sleep(Duration::from_millis((closure.1 + 1_500 - now_ms()).max(0) as u64)).await;
    let stamps: Vec<Millis> = applied
        .lock()
        .expect("recorder")
        .iter()
        .filter_map(|(_, cmd, _)| match cmd {
            Command::Prices { market: AAPL, quotes } => Some(quotes[0].ts),
            _ => None,
        })
        .collect();
    assert!(stamps.iter().any(|ts| *ts > read_ts), "never re-dated before the closure: {stamps:?}");
    assert!(stamps.iter().all(|ts| *ts < closure.0), "re-dated into or past the closure: {stamps:?}");
}

// ── Helpers ──

/// A loopback venue that reads what the feed sends and sends nothing, answering each ping some
/// time after it arrives, until stopped. Books can be sent on it while it runs.
struct Answering {
    /// The host time the newest ping arrived.
    pinged: Arc<AtomicI64>,
    stop: Arc<Notify>,
    task: JoinHandle<()>,
    send: Arc<Mutex<SplitSink<Conn, Message>>>,
}

/// Keeps a stopped venue's connection open while nothing reads or answers it.
struct Silent {
    _send: SplitSink<Conn, Message>,
}

impl Answering {
    /// Sends `messages` to the feed.
    async fn send(&self, messages: &[String]) {
        let mut send = self.send.lock().await;
        for message in messages {
            send.send(Message::text(message.as_str())).await.expect("send");
        }
    }

    /// Stops reading, so no ping is answered from now on, and keeps the connection open.
    async fn stop(self) -> Silent {
        self.stop.notify_one();
        self.task.await.expect("the venue task");
        let send = Arc::try_unwrap(self.send).expect("no other sender").into_inner();
        Silent { _send: send }
    }
}

/// The next quote the feed sequences on `rx`, with when its delay bound runs out, applied to
/// `engine`, which answers the feed's queries meanwhile.
async fn next_quote(
    rx: &mut mpsc::Receiver<Request>,
    engine: &mut FakeEngine,
) -> (SourceQuote, Option<std::time::Instant>) {
    let quote = async {
        loop {
            match rx.recv().await.expect("feed running") {
                Request::Quote { cmd: Command::Prices { market, quotes }, not_after } => {
                    let [quote] = quotes.as_slice() else { panic!("one quote a command: {quotes:?}") };
                    let quote = quote.clone();
                    engine.apply(Command::Prices { market, quotes });
                    return (quote, not_after);
                }
                Request::Query(query) => engine.answer(query),
                _ => {}
            }
        }
    };
    timeout(WAIT, quote).await.expect("a quote in time")
}

/// Answers the pings on `ws` `delay` after each arrives (see `Answering`).
fn answer_pings(ws: Conn, delay: Duration) -> Answering {
    let (send, mut read) = ws.split();
    let (pinged, stop) = (Arc::new(AtomicI64::new(0)), Arc::new(Notify::new()));
    let (noted, stopped) = (pinged.clone(), stop.clone());
    let task = tokio::spawn(async move {
        loop {
            // A ping read queues its answer, which the next read sends.
            tokio::select! {
                () = stopped.notified() => return,
                message = read.next() => match message {
                    Some(Ok(Message::Ping(_))) => {
                        noted.store(now_ms(), Ordering::SeqCst);
                        sleep(delay).await;
                    }
                    Some(Ok(_)) => {}
                    _ => return,
                },
            }
        }
    });
    Answering { pinged, stop, task, send: Arc::new(Mutex::new(send)) }
}

impl Node {
    /// The index of `market` at the engine's latest sequenced time.
    async fn index(&self, market: MarketId) -> Result<Fixed, PriceError> {
        let (reply, answer) = oneshot::channel();
        let query = Box::new(move |engine: &engine_core::Engine| {
            let _ = reply.send(engine.markets()[&market].index(engine.now()).map(|index| index.mid));
        });
        self.tx.send(Request::Query(query)).await.expect("sequencer up");
        answer.await.expect("an answer")
    }
}
