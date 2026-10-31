//! The dev scenario offline: loopback Coinbase, Kraken, and Bitstamp venues price BTC-USD and
//! ETH-USD through the sequencer under the dev genesis. Venues go down one at a time: with one
//! down its quotes age out and the markets fill from the other two; with two down, orders get
//! `no_price`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::event::{Event, Reject};
use engine_core::price::PriceError;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, MarketId, Millis, Side};
use k256::ecdsa::SigningKey;
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, interval, sleep, timeout, timeout_at};

use crate::admin::sign;
use crate::clock::now_ms;
use crate::config::{Genesis, Venue};
use crate::feeds::tests::fake::{
    WAIT, accept, bitstamp_book, coinbase_levels, fast, kraken_book, listen, next_alert, send, spawn_feed_into, stream,
};
use crate::feeds::venue::Timing;
use crate::log::Manifest;
use crate::sequencer::{Request, Sequencer, SequencerConfig, StreamBody, StreamMsg};
use crate::tee::EnclaveKey;
use crate::test_support::{Alert, capture_alerts, dev_config, scratch_dir, sequencer_config, trusted_now};

/// Each fake venue's book: BTC-USD 81250 / 81251 and ETH-USD 2377 / 2377.1.
const BTC: (&str, &str, &str) = ("81250", "81251", "81250.5");
const ETH: (&str, &str, &str) = ("2377", "2377.1", "2377.05");
/// The crypto markets' freshness window in this test's genesis: short so a cut venue ages out
/// within the test, long enough that a busy runner does not age out the live ones.
const MAX_AGE_MS: Millis = 1_000;

/// A sequencer under the dev genesis, and a feed of its stream.
pub(super) struct Node {
    pub(super) tx: mpsc::Sender<Request>,
    pub(super) stream: broadcast::Receiver<Arc<StreamMsg>>,
    pub(super) account: AccountId,
    /// Where the sequencer writes its log.
    pub(super) dir: PathBuf,
}

#[tokio::test]
async fn venues_going_down_age_out_and_the_index_runs_on_the_rest() {
    let mut node = Node::start("staged").await;
    let _coinbase = start_venue(&node, Venue::Coinbase, serve_coinbase).await;
    let kraken = start_venue(&node, Venue::Kraken, serve_kraken).await;
    let bitstamp = start_venue(&node, Venue::Bitstamp, serve_bitstamp).await;
    let every_quote: Vec<(MarketId, &str)> = [1, 2]
        .into_iter()
        .flat_map(|market| ["coinbase", "kraken", "bitstamp"].map(|source| (market, source)))
        .collect();
    node.wait_for_quotes(&every_quote).await;
    node.deposit().await;
    node.assert_both_markets_fill().await;

    // Kraken's host goes dark: its connection drops, reconnects fail, and its last quotes age out.
    // The sequencer stamps with the wall clock, so ageing takes real time.
    kraken.abort();
    sleep(Duration::from_millis(MAX_AGE_MS as u64 + 500)).await;
    node.assert_both_markets_fill().await;

    // With Bitstamp gone too, one fresh venue is left.
    bitstamp.abort();
    sleep(Duration::from_millis(MAX_AGE_MS as u64 + 500)).await;
    for (market, (_, _, mid)) in [(1, BTC), (2, ETH)] {
        let events = node.buy(market, mid).await;
        let stale = Reject::NoPrice { error: PriceError::Stale { fresh: 1, needed: 2 } };
        assert!(
            events.iter().any(|event| matches!(event, Event::OrderRejected { reason, .. } if *reason == stale)),
            "market {market}: {events:?}"
        );
    }
}

#[tokio::test]
async fn a_market_the_engine_does_not_list_is_alerted_once_and_held_back_until_it_is_listed() {
    let (_guard, mut alerts) = capture_alerts();
    // The operator's mid-test listing is the point, and `V39` needs an adopted ceiling before a
    // listing: this test is of the feed path, so it runs under the pre-`V39` rules.
    let cfg = dev_config();
    let mut node = Node::boot_at("unlisted", &cfg.genesis, ProtocolVersion::V38).await;
    // ETH/USD is mapped to a market the engine does not list.
    start_kraken(&node, &[(1, "BTC/USD"), (99, "ETH/USD")], now_ms).await;
    let unlisted = loop {
        let alert = next_alert(&mut alerts).await;
        if alert["alert"] == "venue_market_unlisted" {
            break alert;
        }
    };
    assert_eq!((unlisted["market"].as_str(), unlisted["symbol"].as_str()), ("99", "ETH/USD"));

    // From then on, over several silence windows, no ETH quote reaches the engine to be refused,
    // and ETH is alerted neither again nor as silent.
    assert_eq!(node.prices_within(99, Duration::from_secs(1)).await, 0);
    while let Ok(alert) = alerts.try_recv() {
        assert!(!["venue_market_unlisted", "venue_symbol_silent"].contains(&alert["alert"].as_str()), "{alert:?}");
    }

    // Once the operator lists the market, its quotes go again.
    let mut spec = dev_config().genesis.markets.into_iter().find(|market| market.id == 2).expect("ETH-USD");
    (spec.id, spec.symbol) = (99, "ETH-USD-B".into());
    let operator = dev_config().operator().expect("the dev operator");
    let key = SigningKey::from_slice(&[0x42; 32]).expect("the dev operator key");
    let listing = sign(&key, &operator, AdminCommand::UpsertMarket(spec), 0, now_ms() + 60_000);
    node.submit(Command::SignedAdmin(Box::new(listing))).await;
    node.wait_for_quotes(&[(99, "kraken")]).await;
}

#[tokio::test]
async fn quotes_older_than_the_engines_market_bound_do_not_count_as_quoting() {
    let (_guard, mut alerts) = capture_alerts();
    let node = Node::start("max-age").await;
    // Within the feed's own bound, but older than the engine's `max_age_ms` for the market, as
    // after its price rules are tightened, so the engine ignores them.
    start_kraken(&node, &[(1, "BTC/USD"), (2, "ETH/USD")], || now_ms() - MAX_AGE_MS - 500).await;
    assert_first_silent(&mut alerts, "BTC/USD").await;
}

// ── Helpers ──

impl Node {
    /// Boots a fresh log as the node does, records client auth and activates the latest protocol, then
    /// sequences the committed dev genesis, with `MAX_AGE_MS` on the crypto markets, and runs the
    /// sequencer.
    async fn start(name: &str) -> Self {
        let mut cfg = dev_config();
        for market in cfg.genesis.markets.iter_mut().filter(|market| [1, 2].contains(&market.id)) {
            market.price_rules.max_age_ms = MAX_AGE_MS;
        }
        Node::boot(name, &cfg.genesis).await
    }

    /// Boots a fresh log as the node does, records client auth and activates the latest protocol,
    /// then sequences `genesis` and runs the sequencer.
    pub(super) async fn boot(name: &str, genesis: &Genesis) -> Self {
        Self::boot_at(name, genesis, ProtocolVersion::LATEST).await
    }

    /// `boot`, activating `version`: tests of listing and quoting paths that predate the rules
    /// `version` adds run under those rules.
    async fn boot_at(name: &str, genesis: &Genesis, version: ProtocolVersion) -> Self {
        let dir = scratch_dir(&format!("feeds-{name}"));
        let (stream_tx, stream) = broadcast::channel(65_536);
        let (withdrawals_tx, _) = mpsc::unbounded_channel();
        let (manifest, _) = watch::channel(Manifest::default());
        // The venues sequence thousands of quotes, so one segment holds the whole test.
        let seq_cfg = SequencerConfig {
            access_policy: Some(crate::test_support::access::policy()),
            segment_commands: 1_000_000,
            checkpoint_every: 1_000,
            checkpoint_parent: Default::default(),
            checkpoints: None,
            ..sequencer_config(&dir, manifest)
        };
        let mut seq = Sequencer::recover(seq_cfg, Arc::new(EnclaveKey::generate()), stream_tx, withdrawals_tx)
            .expect("fresh sequencer");
        if version == ProtocolVersion::LATEST {
            seq.activate_protocol().expect("protocol activation");
        } else {
            seq.activate_protocol_at(version).expect("protocol activation");
        }
        seq.genesis(genesis).expect("genesis");
        // Trusted time admits stamps on the system clock, so the queued genesis goes in now.
        seq.trust(trusted_now()).expect("genesis");
        let (tx, rx) = mpsc::channel(65_536);
        tokio::spawn(seq.run(rx));
        let account = crate::test_support::access::account(1);
        Node { tx, stream, account, dir }
    }

    /// Waits until the engine has sequenced a quote from every `(market, source)` pair.
    pub(super) async fn wait_for_quotes(&mut self, expected: &[(MarketId, &str)]) {
        let mut missing: BTreeSet<(MarketId, String)> =
            expected.iter().map(|(market, source)| (*market, (*source).to_owned())).collect();
        let arrivals = async {
            while !missing.is_empty() {
                let message = self.stream.recv().await.expect("stream");
                if let StreamBody::Command(entry) = &message.body
                    && let Command::Prices { market, quotes } = &entry.sequenced.cmd
                {
                    quotes.iter().for_each(|quote| {
                        missing.remove(&(*market, quote.source.clone()));
                    });
                }
            }
        };
        timeout(WAIT, arrivals).await.expect("a quote from every venue in time");
    }

    /// Buys in both markets and checks each fills at the venues' shared mid.
    async fn assert_both_markets_fill(&self) {
        for (market, (_, _, mid)) in [(1, BTC), (2, ETH)] {
            let events = self.buy(market, mid).await;
            let fill = events.iter().find_map(|event| if let Event::Fill(fill) = event { Some(fill) } else { None });
            let fill = fill.unwrap_or_else(|| panic!("market {market} did not fill: {events:?}"));
            assert_eq!(fill.index, mid.parse().unwrap());
        }
    }

    /// Credits the test account.
    pub(super) async fn deposit(&self) {
        let declaration = self.access_command(|engine| crate::test_support::access::declaration(engine, 1)).await;
        assert!(matches!(declaration.as_slice(), [Event::EligibilityAttested { .. }]));
        let events = self
            .access_command(|engine| {
                crate::test_support::access::funding(
                    engine,
                    Some(crate::test_support::access::account(1)),
                    Some(crate::test_support::access::account(1)),
                    "100000".parse().unwrap(),
                    "dev-1",
                )
            })
            .await;
        assert!(events.iter().any(|event| matches!(event, Event::Deposited { .. })), "{events:?}");
    }

    /// A small market buy quoted at `mid`, and the events it produced.
    pub(super) async fn buy(&self, market: MarketId, mid: &str) -> Vec<Event> {
        let kind = OrderKind::Market { quoted: mid.parse().unwrap(), max_slippage_bps: None };
        let qty = "0.001".parse().unwrap();
        let order = OrderRequest {
            account: self.account,
            client_id: 1,
            market,
            side: Side::Buy,
            qty,
            kind,
            reduce_only: false,
            auth: None,
        };
        self.access_command(move |engine| crate::test_support::access::order(engine, 1, order)).await
    }

    /// A one-unit buy of `kind` in `market` by the node's account, and the events it produced.
    pub(super) async fn buy_kind(&self, market: MarketId, kind: OrderKind) -> Vec<Event> {
        let order = OrderRequest {
            account: self.account,
            client_id: 2,
            market,
            side: Side::Buy,
            qty: Fixed::ONE,
            kind,
            reduce_only: false,
            auth: None,
        };
        self.access_command(move |engine| crate::test_support::access::order(engine, 1, order)).await
    }

    /// Builds one explicitly requested signed input from the sequencer's recorded domain/nonce.
    async fn access_command(&self, build: impl FnOnce(&engine_core::Engine) -> Command + Send + 'static) -> Vec<Event> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Request::Query(Box::new(move |engine| {
                let _ = reply.send(build(engine));
            })))
            .await
            .unwrap();
        self.submit(response.await.unwrap()).await
    }

    /// Counts the price commands for `market` the engine applies over `window`, after letting
    /// those already sent drain through.
    pub(super) async fn prices_within(&mut self, market: MarketId, window: Duration) -> usize {
        sleep(Duration::from_millis(100)).await;
        while !matches!(self.stream.try_recv(), Err(TryRecvError::Empty | TryRecvError::Closed)) {}
        let deadline = Instant::now() + window;
        let mut count = 0;
        while let Ok(Ok(message)) = timeout_at(deadline, self.stream.recv()).await {
            if let StreamBody::Command(entry) = &message.body
                && matches!(entry.sequenced.cmd, Command::Prices { market: priced, .. } if priced == market)
            {
                count += 1;
            }
        }
        count
    }

    /// Sequences `cmd` and returns its events.
    pub(super) async fn submit(&self, cmd: Command) -> Vec<Event> {
        let (reply, events) = oneshot::channel();
        self.tx.send(Request::Submit { cmd, reply: Some(reply) }).await.expect("sequencer up");
        timeout(WAIT, events).await.expect("reply in time").expect("reply")
    }
}

/// Starts a Kraken feed into `node` for `symbols` with a wide freshness bound, so only the
/// engine's rules can keep a quote from counting: its BTC quotes are stamped by `btc_at`, its ETH
/// quotes normally.
async fn start_kraken(node: &Node, symbols: &[(MarketId, &str)], btc_at: impl Fn() -> Millis + Send + 'static) {
    let (listener, url) = listen().await;
    tokio::spawn(async move {
        let (ws, _) = accept(&listener).await;
        let books = move || {
            vec![kraken_book("BTC/USD", BTC.0, BTC.1, btc_at()), kraken_book("ETH/USD", ETH.0, ETH.1, now_ms())]
        };
        let _ = stream(ws, books).await;
    });
    let timing = Timing { stale_after: Duration::from_secs(60), ..fast(Duration::from_millis(300)) };
    spawn_feed_into(node.tx.clone(), Venue::Kraken, &url, symbols, timing);
}

/// Waits for the first symbol alerted as silent, and checks it is `symbol`.
async fn assert_first_silent(alerts: &mut mpsc::UnboundedReceiver<Alert>, symbol: &str) {
    let silent = async {
        loop {
            let alert = next_alert(alerts).await;
            if alert["alert"] == "venue_symbol_silent" {
                return alert;
            }
        }
    };
    let alert = timeout(WAIT, silent).await.expect("a symbol alerted as silent");
    assert_eq!(alert["symbol"], symbol);
}

/// Starts `venue`'s feed into `node` against a loopback venue that `serve` runs, and returns the
/// venue's task: aborting it takes the venue down.
async fn start_venue(node: &Node, venue: Venue, serve: fn(TcpListener) -> JoinHandle<()>) -> JoinHandle<()> {
    let symbols: [(MarketId, &str); 2] = match venue {
        Venue::Bitstamp => [(1, "btcusd"), (2, "ethusd")],
        Venue::Coinbase => [(1, "BTC-USD"), (2, "ETH-USD")],
        Venue::Kraken => [(1, "BTC/USD"), (2, "ETH/USD")],
        Venue::Massive | Venue::Tradermade | Venue::Databento => {
            unreachable!("only the crypto venues price BTC and ETH")
        }
    };
    let (listener, url) = listen().await;
    let server = serve(listener);
    spawn_feed_into(node.tx.clone(), venue, &url, &symbols, fast(WAIT));
    server
}

/// Serves Kraken tickers for both markets every 20ms, stamped now.
fn serve_kraken(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (mut ws, _) = accept(&listener).await;
        let mut every = interval(Duration::from_millis(20));
        loop {
            every.tick().await;
            let ts = now_ms();
            let tickers = [kraken_book("BTC/USD", BTC.0, BTC.1, ts), kraken_book("ETH/USD", ETH.0, ETH.1, ts)];
            send(&mut ws, &tickers).await;
        }
    })
}

/// Serves a Coinbase snapshot of both books, then refreshes their levels every 20ms.
fn serve_coinbase(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (mut ws, _) = accept(&listener).await;
        let mut every = interval(Duration::from_millis(20));
        for sequence in (0..u64::MAX).step_by(2) {
            every.tick().await;
            let (ts, snapshot) = (now_ms(), sequence == 0);
            let books = [
                coinbase_levels(sequence, snapshot, "BTC-USD", BTC.0, BTC.1, ts),
                coinbase_levels(sequence + 1, snapshot, "ETH-USD", ETH.0, ETH.1, ts),
            ];
            send(&mut ws, &books).await;
        }
    })
}

/// Serves Bitstamp books for both markets every 20ms, stamped now.
fn serve_bitstamp(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (mut ws, _) = accept(&listener).await;
        let mut every = interval(Duration::from_millis(20));
        loop {
            every.tick().await;
            let ts = now_ms();
            send(&mut ws, &[bitstamp_book("btcusd", BTC.0, BTC.1, ts), bitstamp_book("ethusd", ETH.0, ETH.1, ts)])
                .await;
        }
    })
}
