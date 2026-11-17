//! The US share markets' two TLS sources: loopback stand-ins for Massive's stocks feed and Alpaca's
//! consolidated feed, each behind TLS a test CA certifies. A refused Alpaca login is alerted with no
//! key ID, secret key or text Alpaca wrote in any log line or in what a stopped feed returns; and the
//! dev scenario, where Apple, closed while both vendors quote it, opens and fills at the index of the
//! two sources, goes unpriced when either stops and prices again when it is back, and the log then
//! replays.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use engine_core::Fixed;
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::{Command, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::market::MarketSpec;
use engine_core::price::PriceError;
use engine_core::types::{MarketId, Millis};
use tokio::task::AbortHandle;
use tokio::time::{sleep, timeout};

use crate::clock::now_ms;
use crate::config::{Genesis, Venue, VenueConnection, VenueSymbol};
use crate::feeds::tests::alpaca_support::{AlpacaStandIn, KEY_ID, REFUSAL_TEXT, SECRET_KEY};
use crate::feeds::tests::cme_support::key;
use crate::feeds::tests::fake::{WAIT, fake_sequencer, fast, listen};
use crate::feeds::tests::rwa_support::{Applied, TlsFront, around_the_clock, events_of, listing, priced, wait_for};
use crate::feeds::tests::scenario::Node;
use crate::feeds::tests::vendors::{KEY, Prices, serve_massive};
use crate::feeds::venue::{self, Credentials, Timing};
use crate::replay_path;
use crate::sequencer::Request;
use crate::test_support::dev_config;
use crate::trusted_time::tests::logs::Logs;

const AAPL: MarketId = 40;
/// Massive's Apple mid after the open, 230.01, and Alpaca's, 230.03: two sources' index is their mean.
const INDEX: &str = "230.02";
/// Far enough ahead that setup ends well before it, however long setup takes.
const LATER_MS: Millis = 600_000;

#[tokio::test]
async fn a_refused_login_alerts_without_the_key_pair_or_alpacas_text_in_any_log_line() {
    let (logs, _guard) = Logs::capture();
    let tls = TlsFront::new();
    let stand_in = AlpacaStandIn::start(&tls, prices(Venue::Alpaca, Arc::default())).await;
    // The stand-in answers each login with error 402, its text echoing the key ID and secret key.
    stand_in.refuse_logins();
    let (tx, rx) = fake_sequencer();
    let feed = spawn_alpaca(&stand_in, &tls, fast(Duration::from_secs(1)), tx);
    wait_for(WAIT, || (logs.warnings("venue_feed_down") > 0).then_some(())).await;
    assert!(logs.warnings("venue_message_refused") > 0);
    assert!(logs.count("error 402: auth failed") > 0, "the refusal names its code");
    assert!(logs.count("-byte message, SHA-256 ") > 0, "the refused login was excerpted");

    // With the sequencer gone, the attempt that finds it gone is what the task returns to its
    // supervisor: its login refused, or its first query unanswered, whichever comes first.
    drop(rx);
    let stopped = timeout(WAIT, feed).await.expect("the feed stops").expect("no panic");
    let err = format!("{:#}", stopped.unwrap_err());
    assert!(err.contains("sequencer gone"), "{err}");
    for secret in [KEY_ID, SECRET_KEY, REFUSAL_TEXT] {
        assert_eq!(logs.count(secret), 0, "{secret} reached a log line");
        assert!(!err.contains(secret), "{err}");
    }
    assert_eq!(stand_in.logins(), 0);
    stand_in.abort();
}

#[tokio::test]
#[ignore = "dev scenario, run by scripts/dev-share-feeds.sh"]
async fn dev_scenario_share_feeds() {
    let (logs, _guard) = Logs::capture();
    let start = now_ms();
    let open_at = Arc::new(AtomicI64::new(start + LATER_MS));
    let apple = |open: Millis| MarketSpec { schedule: around_the_clock(Some((start - 60_000, open))), ..listing(AAPL) };
    assert_eq!(apple(start).price_rules.min_sources, 2, "priced from two sources");
    let mut node = Node::boot("shares", &genesis(apple(start + LATER_MS))).await;
    let applied = node.record();

    // 1. Massive's stocks feed and Alpaca's consolidated feed, each over TLS, quote Apple while its
    //    market is closed: at 200 before the open and 230 after, Alpaca 0.02 above Massive.
    let tls = TlsFront::new();
    let massive_quoting = Arc::new(AtomicBool::new(true));
    let alpaca = AlpacaStandIn::start(&tls, prices(Venue::Alpaca, open_at.clone())).await;
    let massive_prices = prices(Venue::Massive, open_at.clone());
    let quoting = massive_quoting.clone();
    let massive: Prices = Arc::new(move |symbol, ts| {
        if !quoting.load(Ordering::SeqCst) {
            return None;
        }
        massive_prices(symbol, ts)
    });
    let mut tasks = start_massive(&node, &tls, massive).await;
    tasks.push(spawn_alpaca(&alpaca, &tls, fast(WAIT), node.tx.clone()).abort_handle());
    node.deposit().await;

    // 2. A buy limit above every quote rests while the market is closed. A relisting opens it two
    //    seconds on, and only quotes after the open, from both sources, fill it at their index.
    let events = node.buy_kind(AAPL, OrderKind::Limit { price: "240".parse().unwrap() }).await;
    assert!(matches!(events[..], [Event::OrderAccepted { .. }]), "{events:?}");
    let opens = now_ms() + 2_000;
    open_at.store(opens, Ordering::SeqCst);
    node.relist(apple(opens)).await;
    let fills = wait_for(WAIT, || Some(filled(&applied)).filter(|fills| !fills.is_empty())).await;
    assert_eq!(fills.iter().map(|(_, index)| *index).collect::<Vec<_>>(), [INDEX.parse::<Fixed>().unwrap()]);
    assert!(fills[0].0 >= opens, "filled at {} before the open at {opens}", fills[0].0);
    assert!(priced(&applied, AAPL).iter().all(|(ts, _)| *ts >= opens), "Apple priced before its open");
    node.wait_for_quotes(&[(AAPL, "massive"), (AAPL, "alpaca")]).await;
    assert_fills_at_the_index(&node).await;
    println!("Apple opened and filled at {INDEX}, the index of Massive's and Alpaca's quotes over TLS");

    // 3. Alpaca stops: its connections end and its reconnects fail, and Apple, with Massive alone,
    //    has no price. Back, it logs in again and Apple prices from both.
    assert_eq!(logs.warnings("venue_feed_down"), 0, "a feed went down before either vendor stopped");
    alpaca.set_stopped(true);
    assert_unpriced(&node).await;
    wait_for(WAIT, || (logs.warnings("venue_feed_down") > 0).then_some(())).await;
    alpaca.set_stopped(false);
    assert_fills_at_the_index(&node).await;
    assert!(alpaca.logins() >= 2, "Alpaca's feed logged in again");
    println!("Alpaca stopped: venue_feed_down, Apple unpriced; back: Apple priced from both");

    // 4. Massive stops quoting, and Apple, with Alpaca alone, has no price until it quotes again.
    massive_quoting.store(false, Ordering::SeqCst);
    assert_unpriced(&node).await;
    massive_quoting.store(true, Ordering::SeqCst);
    assert_fills_at_the_index(&node).await;
    println!("Massive stopped: Apple unpriced; back: Apple priced from both");

    // No quote reached the engine for it to refuse, none from outside the market's hours, and no
    // credential reached a log line.
    let refused: Vec<Event> =
        events_of(&applied).into_iter().filter(|event| matches!(event, Event::PricesRejected { .. })).collect();
    assert!(refused.is_empty(), "{refused:?}");
    for secret in [KEY, KEY_ID, SECRET_KEY] {
        assert_eq!(logs.count(secret), 0, "{secret} reached a log line");
    }

    // 5. With the feeds stopped, the log replays: every root, signature, and event checks.
    tasks.iter().for_each(AbortHandle::abort);
    alpaca.abort();
    node.submit(Command::Tick).await;
    let replayed = replay_path(&node.dir).expect("the log replays");
    assert!(replayed.commands > 0 && replayed.checkpoints > 0);
    println!("the log replays: {} commands, {} checkpoints", replayed.commands, replayed.checkpoints);
}

// ── Helpers ──

/// The dev genesis's crypto markets and `apple`.
fn genesis(apple: MarketSpec) -> Genesis {
    let mut genesis = dev_config().genesis;
    genesis.markets.retain(|market| [1, 2].contains(&market.id));
    genesis.close_spread_ceilings.retain(|ceiling| [1, 2].contains(&ceiling.market));
    genesis.close_spread_ceilings.push(CloseSpreadCeiling { market: AAPL, max_close_spread_bps: 30 });
    genesis.markets.push(apple);
    genesis
}

/// Each vendor's Apple book: 200 before `open_at` and 230 after, Alpaca's 0.02 above Massive's.
fn prices(venue: Venue, open_at: Arc<AtomicI64>) -> Prices {
    Arc::new(move |symbol, ts| {
        let (bid, ask) = match symbol {
            "AAPL" if ts < open_at.load(Ordering::SeqCst) => ("200", "200.02"),
            "AAPL" => ("230", "230.02"),
            _ => return None,
        };
        let shift: Fixed = if venue == Venue::Alpaca { "0.02".parse().unwrap() } else { Fixed::ZERO };
        Some((bid.parse::<Fixed>().unwrap() + shift, ask.parse::<Fixed>().unwrap() + shift))
    })
}

/// Runs an Alpaca feed for Apple against `stand_in` over `tls`, sequencing into `tx`.
fn spawn_alpaca(
    stand_in: &AlpacaStandIn,
    tls: &TlsFront,
    timing: Timing,
    tx: tokio::sync::mpsc::Sender<Request>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let conn = VenueConnection {
        venue: Venue::Alpaca,
        url: stand_in.url.clone(),
        api_key_env: None,
        api_key_secret: None,
        oauth: None,
        key_pair: None,
        symbols: vec![VenueSymbol { market: AAPL, symbol: "AAPL".into(), contract: None }],
    };
    tokio::spawn(venue::run(conn, stand_in.credentials(), timing, tls.client.clone(), tx))
}

/// Starts a loopback Massive stocks feed quoting `prices`, behind TLS from `tls`, and its feed into
/// `node`. Aborting the returned tasks stops them.
async fn start_massive(node: &Node, tls: &TlsFront, prices: Prices) -> Vec<AbortHandle> {
    let (listener, plain) = listen().await;
    let (url, front) = tls.relay(&plain).await;
    let server = serve_massive(listener, prices);
    let conn = VenueConnection {
        venue: Venue::Massive,
        url: format!("{url}/stocks"),
        api_key_env: Some("VENDOR_API_KEY".into()),
        api_key_secret: None,
        oauth: None,
        key_pair: None,
        symbols: vec![VenueSymbol { market: AAPL, symbol: "AAPL".into(), contract: None }],
    };
    let credentials = Credentials::ApiKey(key(KEY));
    let feed = tokio::spawn(venue::run(conn, credentials, fast(WAIT), tls.client.clone(), node.tx.clone()));
    vec![server.abort_handle(), front, feed.abort_handle()]
}

/// A one-share market buy of Apple, quoted at the index.
async fn buy(node: &Node) -> Vec<Event> {
    node.buy_kind(AAPL, OrderKind::Market { quoted: INDEX.parse().unwrap(), max_slippage_bps: None }).await
}

/// Buys Apple until it fills at the two sources' index, as it does once both quote.
async fn assert_fills_at_the_index(node: &Node) {
    let filled = async {
        loop {
            let events = buy(node).await;
            let index =
                events.iter().find_map(|event| if let Event::Fill(fill) = event { Some(fill.index) } else { None });
            match index {
                Some(index) => return index,
                None => sleep(Duration::from_millis(200)).await,
            }
        }
    };
    let index = timeout(WAIT * 2, filled).await.expect("Apple filled in time");
    assert_eq!(index, INDEX.parse().unwrap(), "filled away from the two sources' index");
}

/// Buys Apple until the engine refuses it for want of a second fresh source.
async fn assert_unpriced(node: &Node) {
    let stale = Reject::NoPrice { error: PriceError::Stale { fresh: 1, needed: 2 } };
    let refused = async {
        loop {
            let events = buy(node).await;
            if events.iter().any(|event| matches!(event, Event::OrderRejected { reason, .. } if *reason == stale)) {
                return;
            }
            sleep(Duration::from_millis(200)).await;
        }
    };
    timeout(WAIT, refused).await.expect("Apple unpriced with one source");
}

/// The stamp and index of each Apple fill so far.
fn filled(applied: &Applied) -> Vec<(Millis, Fixed)> {
    let applied = applied.lock().expect("recorder");
    applied
        .iter()
        .flat_map(|(ts, _, events)| events.iter().map(move |event| (*ts, event)))
        .filter_map(|(ts, event)| match event {
            Event::Fill(fill) if fill.market == AAPL => Some((ts, fill.index)),
            _ => None,
        })
        .collect()
}
