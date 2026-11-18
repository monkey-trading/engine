//! Books a venue sends from behind its own time once a ping's answer has re-dated the symbol. A
//! venue can serve a symbol from a source minutes behind while it answers pings at once, as Coinbase
//! did on 16 November 2026. Each such book carries the venue's old time, and the stamp an answer gave
//! the symbol does not make it count: alone or as a stream, on each venue's adapter, on a Coinbase
//! product until it is resubscribed alone, and on one of Kraken's two connections. Through the real
//! sequencer, the market loses the venue rather than pricing or filling at its old book.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::price::{PriceError, SourceQuote};
use engine_core::types::{MarketId, Millis};
use futures_util::future::join_all;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;

use crate::clock::now_ms;
use crate::config::Venue;
use crate::feeds::coinbase::resubscriptions;
use crate::feeds::tests::coinbase_support::{self, CoinbaseVenue};
use crate::feeds::tests::fake::{
    Conn, WAIT, accept, bitstamp_book, coinbase_exchange_book, coinbase_levels, kraken_book, listen, send, spawn_feed,
    spawn_feed_into, stream,
};
use crate::feeds::tests::scenario::Node;
use crate::feeds::venue::{STALE_GRACE, Timing};
use crate::sequencer::{Request, StreamBody};
use crate::test_support::dev_config;

const BTC: [(MarketId, &str); 1] = [(1, "BTC/USD")];
/// `ops/runbooks/production-node.toml`'s `[feeds.venues]`.
const PRODUCTION: Timing = Timing {
    interval: Duration::from_millis(10),
    timeout: Duration::from_secs(10),
    reconnect_min: Duration::from_millis(500),
    reconnect_max: Duration::from_secs(30),
    stale_after: Duration::from_secs(1),
    ping_interval: Duration::from_millis(500),
    max_delay: Duration::from_secs(2),
};
/// How far behind its own book the venue's source has fallen.
const BEHIND_MS: Millis = 10 * 60 * 1_000;
/// The oldest a BTC-USD quote counts at: `stale_after`, and the dev genesis's `max_age_ms`.
const COUNTS_MS: Millis = 1_000;
/// The market now, on both venues: 100 / 101.
const CURRENT_MID: &str = "100.5";
/// What the venue showed ten minutes ago: 100.2 / 101.2, 19 bps away, inside BTC-USD's 25 bps bound.
const OLD: (&str, &str, &str) = ("100.2", "101.2", "100.7");
/// The index with the old book in it.
const TAINTED_INDEX: &str = "100.6";
/// A book the venue sends after its old one, its top inside every other book's so that it is the
/// top on every adapter, Coinbase's updates included. A connection is read in order, so seeing it
/// sequenced shows the old book was read before it.
const LATER: (&str, &str, &str) = ("100.5", "100.8", "100.65");
/// Production's timings, with a read timeout that outlasts a watch over a book from behind, so the
/// connection is not replaced before the book that follows it.
const STANDING: Timing = Timing { timeout: Duration::from_secs(30), ..PRODUCTION };
/// The longest a fill's price may be old under production's values with a stalled book
/// (`docs/security-model.md`: 5 s plus `max_delay` plus `max_age_ms`).
const STATED_BOUND: Duration = Duration::from_secs(8);

/// A venue's book message for BTC-USD: its number on the connection, bid, ask and venue time.
type BookMessage = fn(u64, &str, &str, Millis) -> String;

/// What the engine holds for BTC-USD at its last stamp.
#[derive(Debug)]
struct Look {
    index: Result<Fixed, PriceError>,
    /// Kraken's quote, and whether it counts at the engine's time.
    kraken: Option<(Fixed, Millis, bool)>,
}

#[tokio::test]
async fn a_book_sent_from_far_behind_after_a_redated_one_never_counts_or_fills() {
    let mut node = Node::boot("behind-redated", &dev_config().genesis).await;
    node.deposit().await;
    let (_bitstamp, kraken) = start_venues(&node, kraken_book("BTC/USD", "100", "101", now_ms())).await;
    priced_at(&node, CURRENT_MID).await;
    // Kraken's book stands, so its pings' answers re-date it: the symbol now has a stamp given.
    redated(&node).await;

    // Kraken's source falls ten minutes behind: its next book is one it showed then, stamped then.
    let old_ts = now_ms() - BEHIND_MS;
    kraken.books.send(kraken_book("BTC/USD", OLD.0, OLD.1, old_ts)).expect("venue up");
    let (old_mid, tainted): (Fixed, Fixed) = (OLD.2.parse().unwrap(), TAINTED_INDEX.parse().unwrap());
    let mut counted: Option<(Instant, Instant)> = None;
    let mut fill = None;
    let watch_until = Instant::now() + STATED_BOUND;
    while Instant::now() < watch_until {
        let seen = look(&node).await;
        let old_counts = seen.kraken.is_some_and(|(mid, _, counts)| mid == old_mid && counts);
        if old_counts && seen.index == Ok(tainted) {
            let now = Instant::now();
            counted = Some((counted.map_or(now, |(from, _)| from), now));
            if fill.is_none() {
                let events = node.buy(1, TAINTED_INDEX).await;
                fill =
                    events.into_iter().find_map(|event| if let Event::Fill(fill) = event { Some(fill) } else { None });
            }
        }
        sleep(Duration::from_millis(20)).await;
    }
    let report = counted.map(|(from, to)| {
        format!(
            "a Kraken book stamped {BEHIND_MS} ms before it was sent counted in BTC-USD's index for {:?}, moving it \
             from {CURRENT_MID} to {TAINTED_INDEX}; an order filled at index {:?}",
            to.duration_since(from),
            fill.as_ref().map(|fill| fill.index)
        )
    });
    assert!(report.is_none(), "{}", report.unwrap_or_default());
    // The market lost Kraken rather than taking its old price.
    assert_eq!(look(&node).await.index, Err(PriceError::Stale { fresh: 1, needed: 2 }));
    read_before_a_later_book(&mut node, &kraken).await;
    kraken.task.abort();
}

#[tokio::test]
async fn the_same_book_sent_first_on_a_connection_never_counts() {
    let node = Node::boot("behind-first", &dev_config().genesis).await;
    let old = kraken_book("BTC/USD", OLD.0, OLD.1, now_ms() - BEHIND_MS);
    let (_bitstamp, kraken) = start_venues(&node, old).await;
    let old_mid: Fixed = OLD.2.parse().unwrap();
    let watch_until = Instant::now() + STATED_BOUND;
    let mut held = false;
    while Instant::now() < watch_until {
        let seen = look(&node).await;
        held |= seen.kraken.is_some_and(|(mid, ..)| mid == old_mid);
        assert!(!seen.kraken.is_some_and(|(_, _, counts)| counts), "the old book counted: {seen:?}");
        assert!(seen.index.is_err(), "priced with one current venue: {seen:?}");
        sleep(Duration::from_millis(20)).await;
    }
    assert!(held, "the engine never held the old book, so the test showed nothing");
    assert_eq!(look(&node).await.index, Err(PriceError::Stale { fresh: 1, needed: 2 }));
    kraken.task.abort();
}

#[tokio::test]
async fn an_old_book_on_one_kraken_connection_never_displaces_the_other_connections_current_one() {
    let mut node = Node::boot("behind-two-krakens", &dev_config().genesis).await;
    let current = || kraken_book("BTC/USD", "100", "101", now_ms());
    let (_bitstamp, behind) = start_venues(&node, current()).await;
    let other = start_kraken(&node, current()).await;
    priced_at(&node, CURRENT_MID).await;
    // Both connections' books stand, and their pings' answers re-date them.
    redated(&node).await;

    // One connection's source falls ten minutes behind, while the other's sends its book afresh.
    behind.books.send(kraken_book("BTC/USD", OLD.0, OLD.1, now_ms() - BEHIND_MS)).expect("venue up");
    other.books.send(current()).expect("venue up");
    let old_mid: Fixed = OLD.2.parse().unwrap();
    let watch_until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < watch_until {
        let seen = look(&node).await;
        assert!(seen.kraken.is_none_or(|(mid, ..)| mid != old_mid), "the old book displaced the current one: {seen:?}");
        sleep(Duration::from_millis(20)).await;
    }
    // Kraken's leg stays priced from the current connection.
    priced_at(&node, CURRENT_MID).await;
    read_before_a_later_book(&mut node, &behind).await;
    behind.task.abort();
    other.task.abort();
}

#[tokio::test]
async fn a_stream_of_books_from_far_behind_never_counts_on_any_venue() {
    let cases: [(Venue, &str, BookMessage); 4] = [
        (Venue::Kraken, "BTC/USD", |_, bid, ask, ts| kraken_book("BTC/USD", bid, ask, ts)),
        (Venue::Bitstamp, "btcusd", |_, bid, ask, ts| bitstamp_book("btcusd", bid, ask, ts)),
        // A snapshot, then updates: an update at 100.2 / 101.2 leaves the top at 100.2 / 101.
        (Venue::Coinbase, "BTC-USD", |number, bid, ask, ts| {
            coinbase_levels(number, number == 1, "BTC-USD", bid, ask, ts)
        }),
        // Coinbase Exchange's connection re-dates nothing, so its books keep their stamps throughout.
        (Venue::CoinbaseExchange, "BTC-USD", |_, bid, ask, ts| coinbase_exchange_book("BTC-USD", bid, ask, ts)),
    ];
    let reports = join_all(cases.map(|(venue, symbol, book)| counted_from_behind(venue, symbol, book))).await;
    let counted: Vec<String> = reports.into_iter().flatten().collect();
    assert!(counted.is_empty(), "{counted:#?}");
}

#[tokio::test]
async fn a_coinbase_product_streamed_from_behind_counts_again_only_once_resubscribed_alone() {
    let mut venue = CoinbaseVenue::start(&[coinbase_support::BTC]).await;
    let timing = Timing { timeout: Duration::from_secs(6), ..PRODUCTION };
    let (rx, _feed) = spawn_feed(Venue::Coinbase, &venue.url, &[(1, "BTC-USD")], timing);
    let mut quotes = sequenced(rx);
    venue.next_received().await;
    counting(&mut quotes).await;
    // BTC's book stops changing, so its pings' answers re-date it: a quote stamped after the halt.
    venue.halt("BTC-USD");
    let halted = now_ms();
    let standing = until(&mut quotes, |quote| quote.ts > halted, "a re-dated quote").await.1.mid;

    // Its source falls ten minutes behind, and its book changes again, the stale run resubscribing
    // it alone on the same connection.
    let lagged = now_ms();
    venue.set_lag("BTC-USD", Duration::from_millis(BEHIND_MS as u64));
    venue.resume("BTC-USD");
    let alone = resubscriptions(&["BTC-USD"]);
    assert_eq!(
        [venue.next_received().await, venue.next_received().await],
        [(0, alone[0].clone()), (0, alone[1].clone())]
    );
    // Until then only its standing book counted: no book from behind did. The new subscription's
    // books are stamped once the stale run has spanned its grace.
    let grace = Millis::try_from(STALE_GRACE.as_millis()).expect("in range");
    let changed: Vec<(Millis, SourceQuote)> = drain(&mut quotes)
        .into_iter()
        .filter(|(_, quote)| quote.mid != standing && quote.ts < lagged + grace)
        .collect();
    let counted: Vec<_> = changed.iter().filter(|(received, quote)| received - quote.ts <= COUNTS_MS).collect();
    assert!(counted.is_empty(), "books from behind counted: {counted:?}");
    assert!(changed.iter().any(|(_, quote)| now_ms() - quote.ts > BEHIND_MS / 2), "no book from behind was read");
    // Its new subscription is current, and counts.
    counting(&mut quotes).await;
}

#[tokio::test]
async fn a_book_from_far_behind_first_on_a_new_connection_never_counts() {
    let (listener, url) = listen().await;
    let timing = Timing { reconnect_min: Duration::from_millis(10), ..PRODUCTION };
    let (rx, _feed) = spawn_feed(Venue::Kraken, &url, &BTC, timing);
    let mut quotes = sequenced(rx);
    let (mut ws, _) = accept(&listener).await;
    send(&mut ws, &[kraken_book("BTC/USD", "100", "101", now_ms())]).await;
    let first = serve_standing(ws);
    // The book stands, so pings' answers re-date it, and the stamp they gave it outlives the
    // connection.
    let read_ts = until(&mut quotes, |_| true, "the first book").await.1.ts;
    until(&mut quotes, |quote| quote.ts > read_ts, "a re-dated quote").await;

    // The connection drops, and the next one's first book is from ten minutes behind. Nothing
    // waits on the new connection, so it is sequenced as it is read, and its re-dates after it.
    first.task.abort();
    let (mut ws, _) = accept(&listener).await;
    send(&mut ws, &[kraken_book("BTC/USD", OLD.0, OLD.1, now_ms() - BEHIND_MS)]).await;
    let second = serve_standing(ws);
    let old_mid: Fixed = OLD.2.parse().unwrap();
    let mut old = vec![until(&mut quotes, |quote| quote.mid == old_mid, "the old book").await];
    sleep(Duration::from_secs(1)).await;
    old.extend(drain(&mut quotes).into_iter().filter(|(_, quote)| quote.mid == old_mid));
    let counted: Vec<_> = old.iter().filter(|(received, quote)| received - quote.ts <= COUNTS_MS).collect();
    assert!(counted.is_empty(), "a book from behind counted: {counted:?}");
    second.task.abort();
}

// ── Helpers ──

/// A loopback venue that, after its first book, answers every ping and sends each book queued on
/// `books` a little after its next ping's answer, counting them in `sent`.
struct Standing {
    books: mpsc::UnboundedSender<String>,
    sent: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

/// Starts Bitstamp, streaming the current book every 20 ms, and Kraken, which sends `first` and
/// then only what the test queues, both into `node`.
async fn start_venues(node: &Node, first: String) -> (JoinHandle<()>, Standing) {
    let (bitstamp, bitstamp_url) = listen().await;
    spawn_feed_into(node.tx.clone(), Venue::Bitstamp, &bitstamp_url, &[(1, "btcusd")], PRODUCTION);
    let (ws, _) = accept(&bitstamp).await;
    let bitstamp_books = stream(ws, || vec![bitstamp_book("btcusd", "100", "101", now_ms())]);
    (bitstamp_books, start_kraken(node, first).await)
}

/// Starts a Kraken connection into `node` that sends `first` and then only what the test queues.
async fn start_kraken(node: &Node, first: String) -> Standing {
    let (kraken, kraken_url) = listen().await;
    spawn_feed_into(node.tx.clone(), Venue::Kraken, &kraken_url, &BTC, STANDING);
    let (mut ws, _) = accept(&kraken).await;
    send(&mut ws, &[first]).await;
    serve_standing(ws)
}

/// Serves `ws` as `Standing` describes. A ping read queues its answer, which the next read sends, so
/// a book sent 100 ms after a ping arrives is behind that ping's answer.
fn serve_standing(mut ws: Conn) -> Standing {
    let (books, mut queued) = mpsc::unbounded_channel::<String>();
    let sent = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&sent);
    let task = tokio::spawn(async move {
        let mut due: Option<(Instant, String)> = None;
        loop {
            let send_at = due.as_ref().map_or_else(|| Instant::now() + Duration::from_secs(3_600), |(at, _)| *at);
            tokio::select! {
                message = ws.next() => match message {
                    Some(Ok(Message::Ping(_))) => {
                        if due.is_none() && let Ok(book) = queued.try_recv() {
                            due = Some((Instant::now() + Duration::from_millis(100), book));
                        }
                    }
                    Some(Ok(_)) => {}
                    _ => return,
                },
                () = sleep_until(send_at), if due.is_some() => {
                    let (_, book) = due.take().expect("checked");
                    if ws.send(Message::text(book)).await.is_err() {
                        return;
                    }
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    });
    Standing { books, sent, task }
}

/// Waits until `standing` has sent `count` queued books, failing after `WAIT`.
async fn sent(standing: &Standing, count: usize) {
    timeout(WAIT, async {
        while standing.sent.load(Ordering::SeqCst) < count {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the books queued were not sent in time");
}

/// On `venue`'s adapter for `symbol`, a current book stands across pings until one re-dates it (on
/// a venue that re-dates), then the venue sends a book from ten minutes behind after each of its
/// next four pings, and a current one after them. Describes any quote with an old book's top that
/// counted when sequenced before the current one.
async fn counted_from_behind(venue: Venue, symbol: &str, book: BookMessage) -> Option<String> {
    let (listener, url) = listen().await;
    let (rx, _feed) = spawn_feed(venue, &url, &[(1, symbol)], PRODUCTION);
    let (mut ws, _) = accept(&listener).await;
    let first_ts = now_ms();
    send(&mut ws, &[book(1, "100", "101", first_ts)]).await;
    let standing = serve_standing(ws);
    let mut quotes = sequenced(rx);
    let mut read = vec![until(&mut quotes, |_| true, "the first book").await];
    if venue != Venue::CoinbaseExchange {
        read.push(until(&mut quotes, |quote| quote.ts > first_ts, "a re-dated quote").await);
    }
    for number in 2..6 {
        standing.books.send(book(number, OLD.0, OLD.1, now_ms() - BEHIND_MS)).expect("venue up");
    }
    sent(&standing, 4).await;
    standing.books.send(book(6, LATER.0, LATER.1, now_ms())).expect("venue up");
    let later: Fixed = LATER.2.parse().unwrap();
    read.extend(until_all(&mut quotes, |quote| quote.mid == later, "the book after the old ones").await);
    standing.task.abort();
    let (current, later): (Fixed, Fixed) = (CURRENT_MID.parse().unwrap(), later);
    let counted: Vec<_> = read
        .into_iter()
        .filter(|(received, quote)| ![current, later].contains(&quote.mid) && received - quote.ts <= COUNTS_MS)
        .collect();
    (!counted.is_empty()).then(|| format!("{venue:?} counted books from behind: {counted:?}"))
}

/// The quotes a feed sequences, each with the host time it was sequenced at.
fn sequenced(mut rx: mpsc::Receiver<Request>) -> mpsc::UnboundedReceiver<(Millis, SourceQuote)> {
    let (tx, quotes) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            if let Request::Submit { cmd: Command::Prices { quotes, .. }, .. } = request {
                let received = now_ms();
                quotes.into_iter().for_each(|quote| {
                    let _ = tx.send((received, quote));
                });
            }
        }
    });
    quotes
}

/// Every quote sequenced until the first `matches` takes, inclusive, failing after `WAIT`.
async fn until_all(
    quotes: &mut mpsc::UnboundedReceiver<(Millis, SourceQuote)>,
    matches: impl Fn(&SourceQuote) -> bool,
    what: &str,
) -> Vec<(Millis, SourceQuote)> {
    let mut read = Vec::new();
    let until = async {
        loop {
            let quote = quotes.recv().await.expect("the feed stopped");
            let found = matches(&quote.1);
            read.push(quote);
            if found {
                return;
            }
        }
    };
    timeout(WAIT, until).await.unwrap_or_else(|_| panic!("no quote sequenced in time: {what}"));
    read
}

/// The first quote sequenced that `matches` takes, failing after `WAIT`.
async fn until(
    quotes: &mut mpsc::UnboundedReceiver<(Millis, SourceQuote)>,
    matches: impl Fn(&SourceQuote) -> bool,
    what: &str,
) -> (Millis, SourceQuote) {
    until_all(quotes, matches, what).await.pop().expect("the match")
}

/// Every quote sequenced and not yet read.
fn drain(quotes: &mut mpsc::UnboundedReceiver<(Millis, SourceQuote)>) -> Vec<(Millis, SourceQuote)> {
    std::iter::from_fn(|| quotes.try_recv().ok()).collect()
}

/// Waits for a quote that counted when it was sequenced, failing after `WAIT`.
async fn counting(quotes: &mut mpsc::UnboundedReceiver<(Millis, SourceQuote)>) {
    let counting = async {
        loop {
            let (received, quote) = quotes.recv().await.expect("the feed stopped");
            if received - quote.ts <= COUNTS_MS {
                return;
            }
        }
    };
    timeout(WAIT, counting).await.expect("no quote that counts in time");
}

/// What the engine holds for BTC-USD at its last stamp.
async fn look(node: &Node) -> Look {
    let (reply, answer) = oneshot::channel();
    let query = Box::new(move |engine: &engine_core::Engine| {
        let market = &engine.markets()[&1];
        let (now, max_age_ms) = (engine.now(), market.spec.price_rules.max_age_ms);
        let kraken = market.quotes.get("kraken").map(|quote| (quote.mid, quote.ts, now - quote.ts <= max_age_ms));
        let _ = reply.send(Look { index: market.index(now).map(|index| index.mid), kraken });
    });
    node.tx.send(Request::Query(query)).await.expect("sequencer up");
    answer.await.expect("an answer")
}

/// Waits until the engine holds a Kraken quote for BTC-USD re-dated a second past the first it held.
async fn redated(node: &Node) {
    let first = look(node).await.kraken.expect("a Kraken quote").1;
    timeout(WAIT, async {
        while look(node).await.kraken.is_none_or(|(_, ts, _)| ts < first + 1_000) {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the standing book re-dated");
}

/// Sends a current book after the one from behind on `kraken` and waits until `node` sequences it,
/// which shows the connection read the book from behind before it.
async fn read_before_a_later_book(node: &mut Node, kraken: &Standing) {
    kraken.books.send(kraken_book("BTC/USD", LATER.0, LATER.1, now_ms())).expect("venue up");
    let later: Fixed = LATER.2.parse().unwrap();
    let sequenced = async {
        loop {
            let message = match node.stream.recv().await {
                Ok(message) => message,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => panic!("the sequencer stopped"),
            };
            if let StreamBody::Command(entry) = &message.body
                && let Command::Prices { quotes, .. } = &entry.sequenced.cmd
                && quotes.iter().any(|quote| quote.source == "kraken" && quote.mid == later)
            {
                return;
            }
        }
    };
    timeout(WAIT, sequenced).await.expect("the book after the one from behind was never sequenced");
}

/// Waits until BTC-USD's index is `mid`.
async fn priced_at(node: &Node, mid: &str) {
    let mid: Fixed = mid.parse().unwrap();
    timeout(WAIT, async {
        while look(node).await.index != Ok(mid) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("priced from both venues");
}
