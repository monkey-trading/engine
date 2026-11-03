//! Test support: loopback venue servers that speak each venue's message format, a harness that
//! runs a venue task against one, and the wait for the next alert.

use std::time::Duration;

use chrono::{DateTime, SecondsFormat};
use engine_core::Engine;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Millis};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, accept_async};

use crate::clock::now_ms;
use crate::config::{Venue, VenueConnection, VenueSymbol};
use crate::feeds::kraken;
use crate::feeds::venue::{self, Timing};
use crate::sequencer::{Query, Request};
use crate::test_support::{Alert, dev_config};

/// How long a test waits for anything before failing, so a broken feed never hangs the suite.
pub const WAIT: Duration = Duration::from_secs(5);

/// A fake venue's accepted connection.
pub type Conn = WebSocketStream<TcpStream>;

/// An engine listing the dev genesis markets, applying commands as the sequencer does: in order,
/// stamped with the host clock, never going back.
pub struct FakeEngine {
    engine: Engine,
    next_seq: u64,
}

impl FakeEngine {
    /// An engine with every dev genesis market listed, under `V23` (the last version before access
    /// screening), which stores a quote stamped ahead of the sequenced time as stamped at it, as
    /// the latest version a node's log activates does.
    pub fn new() -> Self {
        let mut fake = FakeEngine { engine: Engine::new(), next_seq: 0 };
        let operator = dev_config().operator().expect("the dev operator key");
        for admin in [
            AdminCommand::SetClientAuth(ClientAuth::Unsigned),
            AdminCommand::SetOperator(operator),
            AdminCommand::ActivateProtocol { version: ProtocolVersion::V23 },
        ] {
            let events = fake.apply(Command::Admin(admin));
            assert!(!events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{events:?}");
        }
        for market in dev_config().genesis.markets {
            fake.apply(Command::Admin(AdminCommand::UpsertMarket(market)));
        }
        fake
    }

    /// Stamps and applies one command.
    pub fn apply(&mut self, cmd: Command) -> Vec<Event> {
        let ts = now_ms().max(self.engine.now());
        let events = self.engine.apply(&Sequenced { seq: self.next_seq, ts, cmd }).expect("applied in order");
        self.next_seq += 1;
        events
    }

    /// Runs a query against the engine as it stands.
    pub fn answer(&self, query: Query) {
        query(&self.engine);
    }
}

/// The next alert, failing the test after `WAIT`.
pub async fn next_alert(alerts: &mut mpsc::UnboundedReceiver<Alert>) -> Alert {
    timeout(WAIT, alerts.recv()).await.expect("no alert in time").expect("alert channel closed")
}

/// Short timings, so reconnects and silence play out in milliseconds. A connection's opening ping,
/// which `accept` answers, keeps it within its delay bound for the test: a test of that bound sets
/// its own.
pub fn fast(timeout: Duration) -> Timing {
    Timing {
        interval: Duration::from_millis(10),
        timeout,
        reconnect_min: Duration::from_millis(10),
        reconnect_max: Duration::from_millis(40),
        stale_after: Duration::from_secs(1),
        ping_interval: Duration::from_secs(60 * 60),
        max_delay: Duration::from_secs(24 * 60 * 60),
    }
}

/// A loopback listener and the `ws://` URL that reaches it.
pub async fn listen() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let url = format!("ws://{}", listener.local_addr().expect("local addr"));
    (listener, url)
}

/// Accepts the next connection, completes the WebSocket handshake, and returns it with the
/// first message the feed sent, its subscription.
pub async fn accept(listener: &TcpListener) -> (Conn, String) {
    let (tcp, _) = timeout(WAIT, listener.accept()).await.expect("no connection in time").expect("accept");
    let mut ws = accept_async(tcp).await.expect("handshake");
    let subscription = next_text(&mut ws).await;
    (ws, subscription)
}

/// The next text message from the feed, answering any ping on the way as reading a stream does.
pub async fn next_text(ws: &mut Conn) -> String {
    loop {
        match timeout(WAIT, ws.next()).await.expect("no message in time").expect("closed").expect("read") {
            Message::Text(text) => return text.as_str().to_owned(),
            Message::Ping(_) => {}
            other => panic!("expected text, got {other:?}"),
        }
    }
}

/// Sends text messages to the feed.
pub async fn send(ws: &mut Conn, messages: &[String]) {
    for message in messages {
        ws.send(Message::text(message.as_str())).await.expect("send");
    }
}

/// Sends `messages()` to the feed every 20ms until it closes the connection, answering its pings.
pub fn stream(mut ws: Conn, messages: impl Fn() -> Vec<String> + Send + 'static) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut every = interval(Duration::from_millis(20));
        loop {
            tokio::select! {
                read = ws.next() => {
                    if !matches!(read, Some(Ok(_))) {
                        return;
                    }
                }
                _ = every.tick() => {
                    for message in messages() {
                        if ws.send(Message::text(message)).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    })
}

/// Runs a venue task for `venue` against `url`, returning what it sequences and its handle. What
/// it sequences is also applied to a `FakeEngine`, which answers its queries.
pub fn spawn_feed(
    venue: Venue,
    url: &str,
    symbols: &[(MarketId, &str)],
    timing: Timing,
) -> (mpsc::Receiver<Request>, JoinHandle<anyhow::Result<()>>) {
    let (feed_tx, mut feed_rx) = mpsc::channel(1024);
    let (tx, rx) = mpsc::channel(1024);
    tokio::spawn(async move {
        let mut engine = FakeEngine::new();
        loop {
            tokio::select! {
                // The test dropped its receiver: hang up, so the feed sees the sequencer gone.
                () = tx.closed() => return,
                request = feed_rx.recv() => match request {
                    Some(Request::Query(query)) => engine.answer(query),
                    Some(Request::Submit { cmd, reply }) => {
                        engine.apply(cmd.clone());
                        if tx.send(Request::Submit { cmd, reply }).await.is_err() {
                            return;
                        }
                    }
                    Some(Request::Quote { cmd, .. }) => {
                        engine.apply(cmd.clone());
                        if tx.send(Request::Submit { cmd, reply: None }).await.is_err() {
                            return;
                        }
                    }
                    Some(Request::CheckpointStatus { .. }) => unreachable!("no checkpoint worker in this test"),
                    Some(Request::WatchBenchmarkSchedule(_)) => unreachable!("only the API watches benchmark schedules"),
                    Some(Request::HouseState(_)) => unreachable!("only the API reads the house state"),
                    Some(Request::ReissueWithdrawal { .. }) => unreachable!("only the API asks for a reissue"),
                    Some(
                        Request::AccountPayouts { .. }
                        | Request::EntitlementClaim { .. }
                        | Request::DistributionClaim { .. },
                    ) => unreachable!("only the API asks for payouts and claims"),
                    Some(Request::FinalCheckpoint(_)) => unreachable!("only a handover asks for the final checkpoint"),
                    Some(Request::TimeSample(_)) => unreachable!("a feed sends no time samples"),
                    Some(Request::Sync(_)) => unreachable!("only custody asks for a log sync"),
                    None => return,
                },
            }
        }
    });
    (rx, spawn_feed_into(feed_tx, venue, url, symbols, timing))
}

/// Runs a venue task for `venue` against `url`, sequencing into `tx`.
pub fn spawn_feed_into(
    tx: mpsc::Sender<Request>,
    venue: Venue,
    url: &str,
    symbols: &[(MarketId, &str)],
    timing: Timing,
) -> JoinHandle<anyhow::Result<()>> {
    let symbols = symbols.iter().map(|(market, symbol)| VenueSymbol {
        market: *market,
        symbol: (*symbol).into(),
        contract: None,
    });
    let conn =
        VenueConnection { venue, url: url.to_owned(), api_key_env: None, dataset: None, symbols: symbols.collect() };
    tokio::spawn(venue::run(conn, None, timing, venue::tls_config().expect("tls config"), tx))
}

/// The next price command the feed sequences, failing the test after `WAIT`.
pub async fn next_prices(rx: &mut mpsc::Receiver<Request>) -> (MarketId, Vec<SourceQuote>) {
    match timeout(WAIT, rx.recv()).await.expect("no command in time").expect("feed stopped") {
        Request::Submit { cmd: Command::Prices { market, quotes }, reply: None } => (market, quotes),
        _ => panic!("expected a price command"),
    }
}

/// The quote a venue's top of book becomes.
pub fn quote(venue: Venue, mid: &str, ts: Millis) -> SourceQuote {
    SourceQuote { source: venue.name().into(), mid: mid.parse().expect("mid"), ts, contract: None }
}

/// RFC 3339 with microseconds, as Coinbase and Kraken write times.
fn rfc3339(ts: Millis) -> String {
    DateTime::from_timestamp_millis(ts).expect("time in range").to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// A Kraken book snapshot carrying several of one symbol's books in one message, each with one
/// level a side and its checksum; prices and sizes are JSON numbers, as Kraken sends them.
pub fn kraken_books(symbol: &str, books: &[(&str, &str, Millis)]) -> String {
    const BID_QTY: &str = "1.50000000";
    const ASK_QTY: &str = "2.50000000";
    let data = books.iter().map(|(bid, ask, ts)| {
        let (bid, ask) = (*bid, *ask);
        let (at, checksum) = (rfc3339(*ts), kraken::checksum([(ask, ASK_QTY), (bid, BID_QTY)].into_iter()));
        format!(
            r#"{{"symbol":"{symbol}","bids":[{{"price":{bid},"qty":{BID_QTY}}}],"asks":[{{"price":{ask},"qty":{ASK_QTY}}}],"checksum":{checksum},"timestamp":"{at}"}}"#
        )
    });
    format!(r#"{{"channel":"book","type":"snapshot","data":[{}]}}"#, data.collect::<Vec<_>>().join(","))
}

/// A Kraken book snapshot with one level a side and its checksum; prices and sizes are JSON
/// numbers, as Kraken sends them.
pub fn kraken_book(symbol: &str, bid: &str, ask: &str, ts: Millis) -> String {
    kraken_books(symbol, &[(bid, ask, ts)])
}

/// A Bitstamp order book with one level a side.
pub fn bitstamp_book(symbol: &str, bid: &str, ask: &str, ts: Millis) -> String {
    let (secs, micros) = (ts / 1_000, ts * 1_000);
    format!(
        r#"{{"data":{{"timestamp":"{secs}","microtimestamp":"{micros}","bids":[["{bid}","1.0"]],"asks":[["{ask}","1.0"]]}},"channel":"order_book_{symbol}","event":"data"}}"#
    )
}

/// A Coinbase `level2` message setting one bid and one offer level. `snapshot` replaces the
/// product's book; otherwise the levels are updates to it.
pub fn coinbase_levels(sequence: u64, snapshot: bool, product: &str, bid: &str, ask: &str, ts: Millis) -> String {
    let (at, kind) = (rfc3339(ts), if snapshot { "snapshot" } else { "update" });
    let level = |side: &str, price: &str| {
        format!(r#"{{"side":"{side}","event_time":"{at}","price_level":"{price}","new_quantity":"1.25"}}"#)
    };
    let (bid, offer) = (level("bid", bid), level("offer", ask));
    format!(
        r#"{{"channel":"l2_data","timestamp":"{at}","sequence_num":{sequence},"events":[{{"type":"{kind}","product_id":"{product}","updates":[{bid},{offer}]}}]}}"#
    )
}
