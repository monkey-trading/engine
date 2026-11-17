//! Venue feeds and what the engine takes: quotes stamped while a market is closed, and quotes for a
//! futures contract the market does not take, are held back rather than sequenced for the engine to
//! refuse, and neither counts as the venue going silent. Each market's open or roll is moved to a
//! few seconds after setup by a signed relisting, so the phases do not depend on how long setup
//! takes.

use std::time::Duration;

use chrono::{DateTime, Utc};
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::futures::FuturesContract;
use engine_core::market::MarketSpec;
use engine_core::types::{MarketId, Millis};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::clock::now_ms;
use crate::config::{Genesis, Venue, VenueConnection, VenueSymbol};
use crate::feeds::tests::fake::{
    accept, bitstamp_book, fast, kraken_book, listen, next_alert, next_text, rfc3339, send, stream,
};
use crate::feeds::tests::rwa_support::{around_the_clock, commands, events_of, listing, priced, wait_for};
use crate::feeds::tests::scenario::Node;
use crate::feeds::venue::{self, Credentials, Timing, tls_config};
use crate::test_support::{Alert, LOADED_WAIT, capture_alerts, dev_config};

const US500: MarketId = 20;
const AAPL: MarketId = 40;
/// Short enough that a symbol counted as silent while its market is closed would alert in the test.
const TIMEOUT: Duration = Duration::from_millis(400);
/// The silence timeout of a feed quoting live, where a test asserts that no alert fires. The venue
/// stand-in quotes every 20 ms, so silence here is the test's runtime thread, which the sequencer,
/// the stand-in and the feed share, stalling: a loaded runner held it past 400 ms and alerted a live symbol. A held-back
/// or closed symbol's count restarts at every check and cannot alert at any timeout, so a long one
/// costs the tests nothing but the waits that make a regression show.
const QUOTING: Duration = Duration::from_secs(2);
/// How long a wait for the feeds or the sequencer lasts. A wait ends the moment its condition holds,
/// so the ceiling only bounds a hang, and a loaded machine is not mistaken for one. Unlike the
/// feeds' own timeout, `TIMEOUT`, it is no part of what the tests check.
const WAIT: Duration = LOADED_WAIT;
/// Pings often enough to come and go within `TIMEOUT`, with a delay bound past it.
const PINGING: Timing = Timing {
    interval: Duration::from_millis(10),
    timeout: TIMEOUT,
    reconnect_min: Duration::from_millis(10),
    reconnect_max: Duration::from_millis(40),
    stale_after: Duration::from_secs(1),
    ping_interval: Duration::from_millis(50),
    max_delay: Duration::from_secs(1),
};
/// The commands after both venues quote June that must not quote December.
const RUN: usize = 200;
/// Far enough ahead that setup ends well before it, however long setup takes.
const LATER_MS: Millis = 600_000;

#[tokio::test]
async fn quotes_stamped_while_a_market_is_closed_are_held_back_without_a_silence_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let start = now_ms();
    let closed = |until| MarketSpec { schedule: around_the_clock(Some((start - 60_000, until))), ..listing(AAPL) };
    let node = Node::boot("closed", &genesis(vec![closed(start + LATER_MS), listing(US500)])).await;
    let applied = node.record();
    start_venue(&node, Venue::Kraken, &[(AAPL, "AAPL", None)]).await;

    // Kraken quotes Apple throughout: through several silence windows, nothing reaches the engine.
    tokio::time::sleep(QUOTING * 2).await;
    assert!(priced(&applied, AAPL).is_empty());
    assert_no_feed_alert(&mut alerts);

    // Apple now opens a second from here; from then, quotes stamped after the open go.
    let open_at = now_ms() + 1_000;
    node.relist(closed(open_at)).await;
    let first = wait_for(WAIT, || priced(&applied, AAPL).first().map(|(ts, _)| *ts)).await;
    assert!(first >= open_at, "priced at {first}, before the open at {open_at}");
    assert!(events_of(&applied).iter().all(|e| !matches!(e, Event::PricesRejected { .. })));
    assert_no_feed_alert(&mut alerts);
}

#[tokio::test]
async fn a_stalled_login_while_every_market_is_closed_alerts_and_reconnects() {
    let (_guard, mut alerts) = capture_alerts();
    let start = now_ms();
    let closed = MarketSpec { schedule: around_the_clock(Some((start - 60_000, start + LATER_MS))), ..listing(AAPL) };
    let node = Node::boot("stalled", &genesis(vec![closed])).await;
    let applied = node.record();
    let (listener, url) = listen().await;
    start_feed(&node, Venue::Tradermade, &[(AAPL, "AAPL", None)], &url);

    // The feed logs in; the vendor completes the handshake and then answers nothing, with Apple
    // closed for the whole test.
    let (_held, login) = accept(&listener).await;
    assert_eq!(login, r#"{"action":"login","fmt":"JSON","key":""}"#);

    // The stall still alerts as an outage and the feed reconnects: the read deadline ignores
    // market hours, so nothing reaches the engine either way.
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_feed_down", "tradermade"));
    assert!(priced(&applied, AAPL).is_empty());
    let (_retry, _) = timeout(WAIT, accept(&listener)).await.expect("no retry in time");
}

#[tokio::test]
async fn a_live_connection_is_kept_by_its_traffic_while_every_market_is_closed() {
    let (_guard, mut alerts) = capture_alerts();
    let start = now_ms();
    let closed = MarketSpec { schedule: around_the_clock(Some((start - 60_000, start + LATER_MS))), ..listing(AAPL) };
    let node = Node::boot("heartbeat", &genesis(vec![closed])).await;
    let (listener, url) = listen().await;
    start_feed(&node, Venue::Kraken, &[(AAPL, "AAPL", None)], &url);

    // The venue answers the subscription with heartbeats only, well inside the read deadline,
    // while Apple is closed for the whole test.
    let (ws, _) = accept(&listener).await;
    stream(ws, || vec![r#"{"channel":"heartbeat"}"#.to_owned()]);

    // The traffic keeps the connection: no outage, no silence, no reconnect over several windows.
    tokio::time::sleep(QUOTING * 2).await;
    assert_no_feed_alert(&mut alerts);
    assert!(timeout(Duration::from_millis(100), listener.accept()).await.is_err(), "reconnected despite traffic");
}

#[tokio::test]
async fn pongs_alone_do_not_keep_a_connection_while_every_market_is_closed() {
    let start = now_ms();
    let closed = MarketSpec { schedule: around_the_clock(Some((start - 60_000, start + LATER_MS))), ..listing(AAPL) };
    let node = Node::boot("pongs-only", &genesis(vec![closed])).await;
    let (listener, url) = listen().await;
    start_feed_timed(&node, Venue::Kraken, &[(AAPL, "AAPL", None)], &url, PINGING);

    // The venue reads on, so every ping is answered, and sends nothing of its protocol. With Apple
    // closed, only the read deadline can end this connection.
    let (mut ws, _) = accept(&listener).await;
    let answering = tokio::spawn(async move { while ws.next().await.is_some() {} });
    let (_retry, _) = timeout(WAIT, accept(&listener)).await.expect("the connection replaced");
    answering.abort();
}

#[tokio::test]
async fn a_ping_unanswered_for_the_timeout_ends_a_connection_while_every_market_is_closed() {
    let start = now_ms();
    let closed = MarketSpec { schedule: around_the_clock(Some((start - 60_000, start + LATER_MS))), ..listing(AAPL) };
    let node = Node::boot("unanswered", &genesis(vec![closed])).await;
    let (listener, url) = listen().await;
    start_feed_timed(&node, Venue::Kraken, &[(AAPL, "AAPL", None)], &url, PINGING);

    // The venue keeps the read deadline fresh with heartbeats, and has stopped reading, so no ping
    // after the opening one is answered. With Apple closed, only the unanswered ping can end this.
    let (mut ws, _) = accept(&listener).await;
    let traffic = tokio::spawn(async move {
        loop {
            send(&mut ws, &[r#"{"channel":"heartbeat"}"#.to_owned()]).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let (_retry, _) = timeout(WAIT, accept(&listener)).await.expect("the connection replaced");
    traffic.abort();
}

#[tokio::test]
async fn contracts_the_market_does_not_take_are_held_back_without_an_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let start = now_ms();
    let futures = |roll_at| {
        let utc = |ts: Millis| DateTime::<Utc>::from_timestamp_millis(ts).expect("in range");
        let contracts = vec![
            FuturesContract { contract: "ESZ6".into(), from: utc(start - 86_400_000) },
            FuturesContract { contract: "ESH7".into(), from: utc(roll_at) },
            FuturesContract { contract: "ESM7".into(), from: utc(start + 2 * LATER_MS) },
        ];
        MarketSpec { schedule: around_the_clock(None), contracts, ..listing(US500) }
    };
    let node = Node::boot("roll", &genesis(vec![listing(AAPL), futures(start + LATER_MS)])).await;
    let applied = node.record();
    let symbols = [(US500, "ESZ6", Some("ESZ6")), (US500, "ESH7", Some("ESH7")), (US500, "ESM7", Some("ESM7"))];
    start_venue(&node, Venue::Kraken, &symbols).await;
    start_venue(&node, Venue::Bitstamp, &symbols).await;

    // December and March, front and next, are sequenced; June, two ahead, is held back.
    let seen = |contract: &str| priced(&applied, US500).iter().any(|(_, c)| c.contains(&Some(contract.into())));
    wait_for(WAIT, || (seen("ESZ6") && seen("ESH7")).then_some(())).await;
    assert!(!seen("ESM7"));
    assert_no_feed_alert(&mut alerts);

    // March rolls in half a second from here. December, rolled off, is held back once the feeds
    // learn of the roll, and June, now next, is sequenced. Each feed learns of it when its next
    // confirmation is answered, and from then sends December no more, so once both venues have
    // quoted June, which nothing sequenced before the roll, no later command quotes December. The
    // waits are for those events, not for a time: they end when they happen, and the ceiling only
    // bounds a hang.
    node.relist(futures(now_ms() + 500)).await;
    let rolled = || {
        commands(&applied)
            .iter()
            .position(|(_, _, events)| events.iter().any(|e| matches!(e, Event::MarketRolled { .. })))
    };
    wait_for(WAIT, rolled).await;
    let quoted_june = |source: &str| {
        commands(&applied).iter().position(|(_, cmd, _)| {
            matches!(cmd, Command::Prices { quotes, .. }
                if quotes.iter().any(|q| q.source == source && q.contract.as_deref() == Some("ESM7")))
        })
    };
    let learned = wait_for(WAIT, || Some(quoted_june("kraken")?.max(quoted_june("bitstamp")?))).await;
    wait_for(WAIT, || (commands(&applied).len() >= learned + RUN).then_some(())).await;
    // December is held back from here, so a window past its last quote shows whether it counts as silent.
    tokio::time::sleep(QUOTING * 3 / 2).await;
    let after = commands(&applied);
    assert!(!contracts(&after[learned..]).contains(&"ESZ6".into()), "December quoted after both feeds learned");
    assert!(events_of(&applied).iter().all(|e| !matches!(e, Event::PricesRejected { .. })));
    assert_no_feed_alert(&mut alerts);
}

#[tokio::test]
async fn a_loopback_venue_serves_the_feed_again_after_it_drops() {
    let url = listen_and_serve(Venue::Kraken, &["ESZ6"]).await;
    for connection in 0..2 {
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connected");
        futures_util::SinkExt::send(&mut ws, tokio_tungstenite::tungstenite::Message::text("subscribe"))
            .await
            .expect("subscribed");
        let book = timeout(Duration::from_secs(10), ws.next()).await;
        assert!(book.is_ok(), "connection {connection} was not served");
    }
}

#[tokio::test]
async fn a_contract_the_listing_does_not_name_is_alerted_once_and_held_back() {
    let (_guard, mut alerts) = capture_alerts();
    let start = now_ms();
    let utc = |ts: Millis| DateTime::<Utc>::from_timestamp_millis(ts).expect("in range");
    let contracts = vec![FuturesContract { contract: "ESZ6".into(), from: utc(start - 86_400_000) }];
    let us500 = MarketSpec { schedule: around_the_clock(None), contracts, ..listing(US500) };
    let node = Node::boot("unnamed", &genesis(vec![listing(AAPL), us500])).await;
    let applied = node.record();
    // The venue quotes December only; the config also names a contract the listing does not.
    start_venue(&node, Venue::Kraken, &[(US500, "ESZ6", Some("ESZ6"))]).await;
    let symbols = [(US500, "ESZ6", Some("ESZ6")), (US500, "ESZ26", Some("ESZ26"))];
    start_feed(&node, Venue::Bitstamp, &symbols, &listen_and_serve(Venue::Bitstamp, &["ESZ6"]).await);

    let unlisted = loop {
        let alert = timeout(WAIT, alerts.recv()).await.expect("an alert in time").expect("alerts");
        assert_ne!(alert["alert"].as_str(), "venue_symbol_silent", "{alert:?}");
        if alert["alert"] == "venue_contract_unlisted" {
            break alert;
        }
    };
    assert_eq!((unlisted["symbol"].as_str(), unlisted["market"].as_str()), ("ESZ26", "20"));
    // Over several silence windows it is alerted neither again nor as silent, and never sequenced.
    tokio::time::sleep(QUOTING * 2).await;
    assert_no_feed_alert(&mut alerts);
    assert!(priced(&applied, US500).iter().all(|(_, contracts)| !contracts.contains(&Some("ESZ26".into()))));
}

/// What a venue that may go quiet sends to log in and confirm its subscription to `AAPL`.
fn quiet_venue_script(venue: Venue) -> [&'static str; 2] {
    match venue {
        Venue::Alpaca => [r#"[{"T":"success","msg":"authenticated"}]"#, r#"[{"T":"subscription","quotes":["AAPL"]}]"#],
        _ => [
            r#"[{"ev":"status","status":"auth_success","message":"authenticated"}]"#,
            r#"[{"ev":"status","status":"success","message":"subscribed to: Q.AAPL"}]"#,
        ],
    }
}

/// Logs a connection in and confirms its subscription as `venue` does, then reads on so pings are
/// answered and the venue sends nothing more. Returns the task that reads.
async fn subscribe_then_stay_quiet(listener: &tokio::net::TcpListener, venue: Venue) -> tokio::task::JoinHandle<()> {
    let [authenticated, subscribed] = quiet_venue_script(venue);
    let (mut ws, _login) = accept(listener).await;
    send(&mut ws, &[authenticated.to_owned()]).await;
    assert!(next_text(&mut ws).await.contains("subscribe"));
    send(&mut ws, &[subscribed.to_owned()]).await;
    tokio::spawn(async move { while ws.next().await.is_some() {} })
}

#[tokio::test]
async fn an_alpaca_connection_that_sends_nothing_outlasts_a_closure_without_alerting() {
    outlasts_a_closure(Venue::Alpaca).await;
}

#[tokio::test]
async fn a_massive_connection_that_sends_nothing_outlasts_a_closure_without_alerting() {
    outlasts_a_closure(Venue::Massive).await;
}

#[tokio::test]
async fn a_quiet_alpaca_connection_has_a_whole_deadline_from_the_open_and_is_replaced_if_silent_after_it() {
    deadline_runs_from_the_open(Venue::Alpaca).await;
}

#[tokio::test]
async fn a_quiet_massive_connection_has_a_whole_deadline_from_the_open_and_is_replaced_if_silent_after_it() {
    deadline_runs_from_the_open(Venue::Massive).await;
}

#[tokio::test]
async fn a_stalled_alpaca_login_while_every_market_is_closed_alerts_and_reconnects() {
    let (_guard, mut alerts) = capture_alerts();
    let start = now_ms();
    let closed = MarketSpec { schedule: around_the_clock(Some((start - 60_000, start + LATER_MS))), ..listing(AAPL) };
    let node = Node::boot("stalled-alpaca", &genesis(vec![closed])).await;
    let (listener, url) = listen().await;
    start_feed_timed(&node, Venue::Alpaca, &[(AAPL, "AAPL", None)], &url, PINGING);

    // The venue completes the handshake and answers nothing, not even the login: the subscription
    // never confirmed, the read deadline still ends the connection.
    let (mut ws, _login) = accept(&listener).await;
    let _reading = tokio::spawn(async move { while ws.next().await.is_some() {} });
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["venue"].as_str()), ("venue_feed_down", "alpaca"));
    timeout(WAIT, accept(&listener)).await.expect("no retry in time");
}

/// A subscribed `venue` connection that sends nothing, its pings answered, stays over several read
/// timeouts while Apple is closed throughout.
async fn outlasts_a_closure(venue: Venue) {
    let (_guard, mut alerts) = capture_alerts();
    let start = now_ms();
    let closed = MarketSpec { schedule: around_the_clock(Some((start - 60_000, start + LATER_MS))), ..listing(AAPL) };
    let node = Node::boot(&format!("quiet-closure-{}", venue.name()), &genesis(vec![closed])).await;
    let (listener, url) = listen().await;
    start_feed_timed(&node, venue, &[(AAPL, "AAPL", None)], &format!("{url}/stocks"), PINGING);
    let _reading = subscribe_then_stay_quiet(&listener, venue).await;
    tokio::time::sleep(TIMEOUT * 4).await;
    assert_no_feed_alert(&mut alerts);
    assert!(timeout(Duration::from_millis(100), listener.accept()).await.is_err(), "reconnected");
}

/// A `venue` connection quiet through a closure and silent after Apple opens is replaced and
/// alerted, but not before a whole read deadline has run from the open.
async fn deadline_runs_from_the_open(venue: Venue) {
    let (_guard, mut alerts) = capture_alerts();
    let start = now_ms();
    let closed = |until| MarketSpec { schedule: around_the_clock(Some((start - 60_000, until))), ..listing(AAPL) };
    let node = Node::boot(&format!("quiet-open-{}", venue.name()), &genesis(vec![closed(start + LATER_MS)])).await;
    let (listener, url) = listen().await;
    start_feed_timed(&node, venue, &[(AAPL, "AAPL", None)], &format!("{url}/stocks"), PINGING);
    let _reading = subscribe_then_stay_quiet(&listener, venue).await;
    // Quiet past several deadlines with Apple closed, then Apple opens a moment from here.
    tokio::time::sleep(TIMEOUT * 3).await;
    let open_at = now_ms() + 300;
    node.relist(closed(open_at)).await;

    // The symbol may be alerted silent first; the connection is then replaced and alerted down.
    let ended = loop {
        let alert = next_alert(&mut alerts).await;
        if alert["alert"].as_str() == "venue_feed_down" {
            assert_eq!(alert["venue"].as_str(), venue.name());
            break now_ms();
        }
    };
    let deadline = i64::try_from(TIMEOUT.as_millis()).unwrap();
    assert!(ended >= open_at + deadline * 9 / 10, "ended {}ms after the open", ended - open_at);
    timeout(WAIT, accept(&listener)).await.expect("no retry in time");
}

/// An Alpaca connection that quoted while Apple was open, was quiet through a closure, and is silent
/// after Apple reopens is replaced within a read deadline of the open, and alerted. The no-fresh-quote
/// count is off for a connection that has quoted, so what ends it is the symbol's silence count,
/// which waits through the closure and runs from the open (`venue_symbol_silent`); the read deadline,
/// reset at the open, ends it only if that count cannot (no symbol it quotes left to be silent).
#[tokio::test]
async fn a_connection_that_quoted_and_then_goes_silent_after_the_open_is_replaced_within_a_deadline() {
    let (_guard, mut alerts) = capture_alerts();
    let open_all = || MarketSpec { schedule: around_the_clock(None), ..listing(AAPL) };
    let closed = |until| MarketSpec { schedule: around_the_clock(Some((now_ms() - 60_000, until))), ..listing(AAPL) };
    let node = Node::boot("quoted-then-silent", &genesis(vec![open_all()])).await;
    let applied = node.record();
    let (listener, url) = listen().await;
    start_feed_timed(&node, Venue::Alpaca, &[(AAPL, "AAPL", None)], &url, PINGING);

    // Logged in and subscribed, the venue quotes every 20ms until told to stop, its pings answered.
    let (mut ws, _login) = accept(&listener).await;
    send(&mut ws, &[quiet_venue_script(Venue::Alpaca)[0].to_owned()]).await;
    assert!(next_text(&mut ws).await.contains("subscribe"));
    send(&mut ws, &[quiet_venue_script(Venue::Alpaca)[1].to_owned()]).await;
    let quoting = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let live = quoting.clone();
    let _serving = stream(ws, move || {
        let at = rfc3339(now_ms());
        let quote = format!(r#"[{{"T":"q","S":"AAPL","bp":230,"bs":3,"ap":230.02,"as":2,"t":"{at}"}}]"#);
        if live.load(std::sync::atomic::Ordering::SeqCst) { vec![quote] } else { Vec::new() }
    });
    wait_for(WAIT, || priced(&applied, AAPL).first().map(|_| ())).await;
    tokio::time::sleep(TIMEOUT / 2).await;

    // Apple closes and the venue falls silent: several deadlines pass with the connection kept.
    quoting.store(false, std::sync::atomic::Ordering::SeqCst);
    node.relist(closed(now_ms() + LATER_MS)).await;
    tokio::time::sleep(TIMEOUT * 3).await;
    assert_no_feed_alert(&mut alerts);
    assert!(timeout(Duration::from_millis(100), listener.accept()).await.is_err(), "reconnected in the closure");

    // Apple reopens a moment from here and the venue stays silent, its pings answered.
    let open_at = now_ms() + 300;
    node.relist(closed(open_at)).await;
    let (_retry, _) = timeout(WAIT, accept(&listener)).await.expect("the silent connection was not replaced");
    let replaced = now_ms();
    let deadline = i64::try_from(TIMEOUT.as_millis()).unwrap();
    assert!(replaced >= open_at + deadline * 9 / 10, "replaced {}ms after the open", replaced - open_at);
    assert_eq!(next_alert(&mut alerts).await["alert"].as_str(), "venue_symbol_silent");
}

// ── Helpers ──

/// The dev genesis's crypto markets and `markets`.
fn genesis(markets: Vec<MarketSpec>) -> Genesis {
    let mut genesis = dev_config().genesis;
    genesis.markets.retain(|market| [1, 2].contains(&market.id));
    genesis.close_spread_ceilings.retain(|ceiling| [1, 2].contains(&ceiling.market));
    // Every listed market takes a ceiling, or the genesis registry refuses to install.
    genesis
        .close_spread_ceilings
        .extend(markets.iter().map(|market| CloseSpreadCeiling { market: market.id, max_close_spread_bps: 30 }));
    genesis.markets.extend(markets);
    genesis
}

/// Starts a loopback `venue` quoting each symbol every 20ms, stamped now, and its feed into `node`.
async fn start_venue(node: &Node, venue: Venue, symbols: &[(MarketId, &str, Option<&str>)]) {
    let names: Vec<&str> = symbols.iter().map(|(_, symbol, _)| *symbol).collect();
    let url = listen_and_serve(venue, &names).await;
    start_feed(node, venue, symbols, &url);
}

/// A loopback `venue` quoting `names` every 20ms, stamped now, and the URL that reaches it. It
/// serves every connection the feed makes: a feed starved on a loaded runner past its read timeout
/// reconnects, and a venue that served only the first would leave it with no quotes for good.
async fn listen_and_serve(venue: Venue, names: &[&str]) -> String {
    let (listener, url) = listen().await;
    let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    tokio::spawn(async move {
        loop {
            let (ws, _) = accept(&listener).await;
            let names = names.clone();
            let books = move || {
                names
                    .iter()
                    .map(|symbol| match venue {
                        Venue::Bitstamp => bitstamp_book(symbol, "100", "101", now_ms()),
                        _ => kraken_book(symbol, "100", "101", now_ms()),
                    })
                    .collect()
            };
            let _ = stream(ws, books).await;
        }
    });
    url
}

/// Starts `venue`'s feed for `symbols` into `node`, reading the venue at `url`.
fn start_feed(node: &Node, venue: Venue, symbols: &[(MarketId, &str, Option<&str>)], url: &str) {
    start_feed_timed(node, venue, symbols, url, fast(QUOTING));
}

/// `start_feed` with `timing`.
fn start_feed_timed(node: &Node, venue: Venue, symbols: &[(MarketId, &str, Option<&str>)], url: &str, timing: Timing) {
    let url = url.to_owned();
    let symbols = symbols
        .iter()
        .map(|(market, symbol, contract)| VenueSymbol {
            market: *market,
            symbol: (*symbol).into(),
            contract: contract.map(Into::into),
        })
        .collect();
    let conn =
        VenueConnection { venue, url, api_key_env: None, api_key_secret: None, oauth: None, key_pair: None, symbols };
    tokio::spawn(venue::run(conn, Credentials::Public, timing, tls_config().expect("tls config"), node.tx.clone()));
}

/// The contracts the quotes of `applied` name, in order.
fn contracts(applied: &[(Millis, Command, Vec<Event>)]) -> Vec<String> {
    applied
        .iter()
        .flat_map(|(_, cmd, _)| match cmd {
            Command::Prices { quotes, .. } => quotes.iter().filter_map(|quote| quote.contract.clone()).collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// Fails on any alert a venue raises for a silent symbol, an outage, or a symbol it holds back.
fn assert_no_feed_alert(alerts: &mut mpsc::UnboundedReceiver<Alert>) {
    let names = ["venue_symbol_silent", "venue_feed_down", "venue_market_unlisted", "venue_contract_unlisted"];
    while let Ok(alert) = alerts.try_recv() {
        assert!(!names.contains(&alert["alert"].as_str()), "{alert:?}");
    }
}
