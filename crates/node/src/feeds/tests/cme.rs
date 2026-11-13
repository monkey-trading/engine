//! CME Group's feed against loopback stand-ins for its token endpoint and market data stream, each
//! reached over TLS a test CA certifies: a connection replaced on a fresh token with no gap in its
//! quotes though the replacement's login is slow, also while it quotes nothing, and kept when the
//! replacement fails until one on a new token takes over; a quiet connection kept through a closure
//! by heartbeats slower than the timeout; a refused token alerted with no credential, token or text
//! the venue wrote in any log line or in what a stopped feed returns; and the dev scenario, where
//! US 500 keeps its price from CME and Massive through the sequencer across token rollovers until
//! the token endpoint refuses, and the log then replays.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::DateTime;
use engine_core::Fixed;
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::Command;
use engine_core::event::{Event, Reject};
use engine_core::futures::FuturesContract;
use engine_core::market::MarketSpec;
use engine_core::price::PriceError;
use engine_core::types::{MarketId, Millis};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::{Instant, sleep, timeout};

use crate::clock::now_ms;
use crate::config::{Genesis, Venue, VenueConnection, VenueSymbol};
use crate::feeds::tests::cme_support::{Answer, CLIENT_ID, CLIENT_SECRET, CmeStandIn, key};
use crate::feeds::tests::fake::{WAIT, fake_sequencer, fast, listen, next_alert};
use crate::feeds::tests::rwa_support::{Applied, TlsFront, around_the_clock, listing, wait_for};
use crate::feeds::tests::scenario::Node;
use crate::feeds::tests::vendors::{KEY, Prices, serve_massive};
use crate::feeds::venue::{self, Credentials, Timing};
use crate::replay_path;
use crate::sequencer::Request;
use crate::test_support::{capture_alerts, dev_config};
use crate::trusted_time::tests::logs::Logs;

const US500: MarketId = 20;
/// Massive's US 500 mid, 6000.5, and CME's, 6001: two sources' index is their mean.
const INDEX: &str = "6000.75";
/// The longest a CME quote may follow the last in the dev scenario and keep its market priced: under
/// the listings' `max_age_ms` of a second, less a margin for the sequencer.
const MAX_GAP: Duration = Duration::from_millis(900);
/// How late the dev scenario's stand-in answers each login after the first: past `MAX_GAP`, so a
/// connection dropped before its replacement is ready leaves a gap the scenario sees.
const LOGIN_DELAY: Duration = Duration::from_millis(1_500);
/// The longest two quotes may be apart in the rollover tests, which share the machine with the rest
/// of the suite: three of CME's 500 ms restatements.
const LOADED_GAP: Duration = Duration::from_millis(1_500);
/// How late those tests' stand-in answers a replacement: past `LOADED_GAP` and `max_delay` both.
const SLOW: Duration = Duration::from_millis(2_500);

#[tokio::test]
async fn a_connection_is_replaced_on_a_fresh_token_with_no_gap_though_the_new_login_is_slow() {
    let (_guard, mut alerts) = capture_alerts();
    let tls = TlsFront::new();
    // Sixteen-second tokens are refreshed at twelve, so a replacement ready a slow login later still
    // takes over before the old token expires.
    let stand_in = CmeStandIn::start(&tls, 16, prices(Venue::Cme)).await;
    stand_in.state.lock().expect("stand-in").login_delay = Some(SLOW);
    let timing = Timing { timeout: Duration::from_secs(4), ..cme_timing() };
    assert_no_gap_across_a_rollover(&stand_in, &tls, timing).await;
    let (sessions, issued, expired) =
        stand_in.seen(|seen| (seen.sessions.clone(), seen.issued.clone(), seen.expired_sessions));
    assert_eq!(expired, 0, "a session outlived its token");
    // Each session opened on its own token, the first fetched at the start and each next ahead.
    let tokens: Vec<String> = issued.into_iter().map(|(token, _)| token).collect();
    assert_eq!(sessions, tokens[..sessions.len()].to_vec());
    assert!(alerts.try_recv().is_err(), "a rollover alerted");
    stand_in.stop();
}

#[tokio::test]
async fn a_refused_token_alerts_without_a_credential_or_token_in_any_log_line() {
    let (logs, _guard) = Logs::capture();
    let tls = TlsFront::new();
    let stand_in = CmeStandIn::start(&tls, 2, prices(Venue::Cme)).await;
    // The endpoint refuses, echoing the client ID, its secret, and the Basic credentials.
    stand_in.answer(Answer::Refuse);
    let (tx, _rx) = fake_sequencer();
    let timing = Timing { timeout: Duration::from_secs(1), ..cme_timing() };
    let _feed = spawn_cme(&stand_in, &tls, &[(1, "ES:202612", None)], timing, tx);
    wait_for(WAIT, || (logs.warnings("venue_feed_down") > 0).then_some(())).await;
    assert!(logs.count("token request refused (401 Unauthorized): invalid_client") > 0);

    // Granted, but the stream refuses each login, echoing the token it was sent.
    stand_in.state.lock().expect("stand-in").refuse_logins = usize::MAX;
    stand_in.answer(Answer::Grant(2));
    wait_for(WAIT, || (logs.warnings("venue_message_refused") > 0).then_some(())).await;
    assert!(logs.count("login refused") > 0);

    let tokens: Vec<String> = stand_in.seen(|seen| seen.issued.iter().map(|(token, _)| token.clone()).collect());
    assert!(!tokens.is_empty());
    let basic = STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"));
    for secret in [CLIENT_ID, CLIENT_SECRET, basic.as_str()].into_iter().chain(tokens.iter().map(String::as_str)) {
        assert_eq!(logs.count(secret), 0, "{secret} reached a log line");
    }
    // Neither the endpoint's text nor the refused login's reaches a log line.
    assert_eq!(logs.count("Invalid client or client credentials"), 0, "the endpoint's text was logged");
    assert!(logs.count("-byte message, SHA-256 ") > 0, "the refused login was excerpted");
    stand_in.stop();
}

#[tokio::test]
async fn a_quiet_connection_outlives_the_timeout_through_a_closure_on_heartbeats() {
    let (_guard, mut alerts) = capture_alerts();
    let node = Node::boot("cme-heartbeats", &genesis(closed_us500())).await;
    let tls = TlsFront::new();
    let stand_in = CmeStandIn::start(&tls, 60, prices(Venue::Cme)).await;
    // CME's ten-second heartbeat against the template's ten-second timeout, scaled down.
    stand_in.state.lock().expect("stand-in").heartbeat_every = Some(Duration::from_millis(1_500));
    let timing = Timing { timeout: Duration::from_secs(1), ..cme_timing() };
    let _feed = spawn_cme(&stand_in, &tls, &[(US500, "ES:202612", Some("ESZ6"))], timing, node.tx.clone());

    sleep(Duration::from_secs(5)).await;
    assert_eq!(stand_in.seen(|seen| seen.sessions.len()), 1, "the quiet connection was replaced");
    assert!(alerts.try_recv().is_err(), "a quiet closure alerted");
    stand_in.stop();
}

#[tokio::test]
async fn a_rollover_on_a_connection_that_quotes_nothing_switches_without_an_outage() {
    let (_guard, mut alerts) = capture_alerts();
    let node = Node::boot("cme-quiet-rollover", &genesis(closed_us500())).await;
    let tls = TlsFront::new();
    // A closure sends only heartbeats: each replacement is ready a ping interval after its ready
    // ping is answered, with no book, well within the second from its predecessor's refresh to the
    // expiry of its four-second token.
    let stand_in = CmeStandIn::start(&tls, 4, prices(Venue::Cme)).await;
    stand_in.state.lock().expect("stand-in").heartbeat_every = Some(Duration::from_millis(400));
    let timing = Timing { timeout: Duration::from_secs(1), ..cme_timing() };
    let _feed = spawn_cme(&stand_in, &tls, &[(US500, "ES:202612", Some("ESZ6"))], timing, node.tx.clone());

    wait_for(WAIT * 2, || (stand_in.seen(|seen| seen.sessions.len()) >= 3).then_some(())).await;
    assert!(alerts.try_recv().is_err(), "a rollover alerted");
    assert_eq!(stand_in.seen(|seen| seen.expired_sessions), 0);
    stand_in.stop();
}

#[tokio::test]
async fn a_replacement_waits_for_its_subscription_past_a_heartbeat_and_a_message_it_refuses() {
    let (_guard, mut alerts) = capture_alerts();
    let tls = TlsFront::new();
    let stand_in = CmeStandIn::start(&tls, 16, prices(Venue::Cme)).await;
    // The replacement's subscription is answered late, after a heartbeat and a message of a type the
    // feed does not take: switching on either, or failing on the second, leaves a gap.
    stand_in.state.lock().expect("stand-in").subscribe_delay = Some(SLOW);
    let timing = Timing { timeout: Duration::from_secs(4), ..cme_timing() };
    assert_no_gap_across_a_rollover(&stand_in, &tls, timing).await;
    assert_eq!(stand_in.seen(|seen| seen.sessions.len()), 2, "the replacement failed and the feed reconnected");
    // The message it passed over is refused once the feed switches, as on any connection.
    let refused = next_alert(&mut alerts).await;
    assert_eq!(refused["alert"], "venue_message_refused");
    assert!(refused["message"].contains("unexpected message type TRD"), "{refused:?}");
    assert!(alerts.try_recv().is_err(), "the rollover alerted otherwise");
    stand_in.stop();
}

#[tokio::test]
async fn a_connection_ends_when_its_token_expires_while_its_replacement_still_opens() {
    let tls = TlsFront::new();
    // Three-second tokens are refreshed at two and a quarter; the replacement's login, answered 1.9 s
    // late, is not ready before the first token expires, and the stand-in keeps the expired session
    // open: the feed must end it.
    let stand_in = CmeStandIn::start(&tls, 3, prices(Venue::Cme)).await;
    {
        let mut state = stand_in.state.lock().expect("stand-in");
        (state.login_delay, state.keep_expired) = (Some(Duration::from_millis(1_900)), true);
    }
    let timing = Timing { timeout: Duration::from_secs(2), ..cme_timing() };
    assert_feed_ends_the_first_session_at_its_expiry(&stand_in, &tls, timing).await;
    stand_in.stop();
}

#[tokio::test]
async fn a_connection_ends_when_its_token_expires_with_no_fresh_one() {
    let tls = TlsFront::new();
    let stand_in = CmeStandIn::start(&tls, 3, prices(Venue::Cme)).await;
    stand_in.state.lock().expect("stand-in").keep_expired = true;
    let timing = Timing { timeout: Duration::from_secs(2), ..cme_timing() };
    let first = async {
        wait_for(WAIT, || (stand_in.seen(|seen| seen.sessions.len()) == 1).then_some(())).await;
        // Every refresh is refused from now on.
        stand_in.answer(Answer::Refuse);
    };
    tokio::join!(first, assert_feed_ends_the_first_session_at_its_expiry(&stand_in, &tls, timing));
    stand_in.stop();
}

#[tokio::test]
async fn a_failed_replacement_keeps_the_working_connection_and_retries_on_a_new_token() {
    let (_guard, mut alerts) = capture_alerts();
    let tls = TlsFront::new();
    let stand_in = CmeStandIn::start(&tls, 16, prices(Venue::Cme)).await;
    let timing = Timing { timeout: Duration::from_secs(4), ..cme_timing() };
    // The first replacement's login is refused: the connection streams on, and the next replacement
    // opens on a new token.
    let refuse_the_first_replacement = async {
        wait_for(WAIT, || (stand_in.seen(|seen| seen.sessions.len()) == 1).then_some(())).await;
        stand_in.state.lock().expect("stand-in").refuse_logins = 1;
    };
    tokio::join!(refuse_the_first_replacement, assert_no_gap_across_a_rollover(&stand_in, &tls, timing));
    let (sessions, issued) = stand_in.seen(|seen| (seen.sessions.clone(), seen.issued.clone()));
    let tokens: Vec<String> = issued.into_iter().map(|(token, _)| token).collect();
    assert_eq!(sessions, [tokens[0].clone(), tokens[2].clone()], "the refused token was used again");
    let (ended, subscribed) = stand_in.seen(|seen| (seen.ended[0].1, seen.subscribed_at[1]));
    assert!(ended >= subscribed, "the working connection ended before its replacement subscribed");
    assert!(alerts.try_recv().is_err(), "the failed replacement alerted");
    stand_in.stop();
}

#[tokio::test]
async fn a_feed_stopped_with_a_refused_token_returns_no_credential() {
    let tls = TlsFront::new();
    let stand_in = CmeStandIn::start(&tls, 2, prices(Venue::Cme)).await;
    stand_in.answer(Answer::Refuse);
    let (tx, rx) = fake_sequencer();
    let timing = Timing { timeout: Duration::from_secs(1), ..cme_timing() };
    let feed = spawn_cme(&stand_in, &tls, &[(1, "ES:202612", None)], timing, tx);
    wait_for(WAIT, || (!stand_in.seen(|seen| seen.requests.is_empty())).then_some(())).await;

    // With the sequencer gone, the next refused request is what the task returns to its supervisor.
    drop(rx);
    let stopped = timeout(WAIT, feed).await.expect("the feed stops").expect("no panic");
    let err = format!("{:#}", stopped.unwrap_err());
    assert!(err.contains("sequencer gone") && err.contains("invalid_client"), "{err}");
    let basic = STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"));
    for secret in [CLIENT_ID, CLIENT_SECRET, basic.as_str()] {
        assert!(!err.contains(secret), "{err}");
    }
    stand_in.stop();
}

#[tokio::test]
#[ignore = "dev scenario, run by scripts/dev-cme-feeds.sh"]
async fn dev_scenario_cme_feeds() {
    let (logs, _guard) = Logs::capture();
    let start = now_ms();
    let contracts = vec![contract("ESZ6", start - 86_400_000), contract("ESH7", start + 86_400_000)];
    let us500 = MarketSpec { schedule: around_the_clock(None), contracts, ..listing(US500) };
    assert_eq!(us500.price_rules.min_sources, 2, "priced from two sources");
    let mut node = Node::boot("cme", &genesis(us500)).await;
    let applied = node.record();

    // 1. The stand-in issues twelve-second tokens and answers each login after the first late; the
    //    feed connects on one and subscribes to the product of US 500's contracts, ES, whole.
    let tls = TlsFront::new();
    // Refreshed at nine seconds, so a replacement has three to log in late, subscribe, and send a
    // book its ready ping bounds.
    let stand_in = CmeStandIn::start(&tls, 12, prices(Venue::Cme)).await;
    stand_in.state.lock().expect("stand-in").login_delay = Some(LOGIN_DELAY);
    let cme_symbols = [(US500, "ES:202612", Some("ESZ6")), (US500, "ES:202703", Some("ESH7"))];
    let mut tasks = vec![spawn_cme(&stand_in, &tls, &cme_symbols, cme_timing(), node.tx.clone()).abort_handle()];
    tasks.extend(start_massive(&node, &tls).await);

    // 2. Its tops of book, every 500 ms, and Massive's price the market from both.
    node.wait_for_quotes(&[(US500, "massive"), (US500, "cme")]).await;
    node.deposit().await;
    assert_fills_at_the_index(&node).await;
    let subscribed = stand_in.seen(|seen| seen.subscriptions[0]["payload"]["subscriptions"].clone());
    assert_eq!(subscribed[0]["productCode"], "ES");
    assert!(subscribed[0].get("periodCodes").is_none(), "{subscribed}");

    // 3. Each token is refreshed ahead of expiry and a replacement takes over on the next, twice,
    //    while US 500 fills at the two sources' index throughout, and on for a second after the
    //    second switch.
    let rollovers = async {
        while logs.count("venue switched to a connection on a fresh bearer token") < 2 {
            assert_fills_at_the_index(&node).await;
            sleep(Duration::from_millis(250)).await;
        }
        for _ in 0..4 {
            assert_fills_at_the_index(&node).await;
            sleep(Duration::from_millis(250)).await;
        }
    };
    timeout(WAIT * 8, rollovers).await.expect("two rollovers in time");
    assert_eq!(stand_in.seen(|seen| seen.expired_sessions), 0, "a session outlived its token");
    let gap = longest_gap(&applied, "cme");
    assert!(gap < MAX_GAP.as_millis() as Millis, "a {gap} ms gap in CME's quotes");
    println!("token rollovers: 2; longest gap between CME's quotes: {gap} ms");

    // 4. The endpoint refuses: the refresh's first refusal alerts while the token in hand still
    //    streams; once it expires the reconnect has none, the outage alerts, and the market, with
    //    Massive alone, has no price.
    stand_in.answer(Answer::Refuse);
    wait_for(WAIT * 4, || (logs.warnings("venue_token_failed") > 0).then_some(())).await;
    assert_eq!(logs.warnings("venue_feed_down"), 0, "the feed went down before its token expired");
    wait_for(WAIT * 4, || (logs.warnings("venue_feed_down") > 0).then_some(())).await;
    let stale = Reject::NoPrice { error: PriceError::Stale { fresh: 1, needed: 2 } };
    let refused = async {
        loop {
            let events = node.buy(US500, INDEX).await;
            if events.iter().any(|event| matches!(event, Event::OrderRejected { reason, .. } if *reason == stale)) {
                return;
            }
            sleep(Duration::from_millis(200)).await;
        }
    };
    timeout(WAIT, refused).await.expect("US 500 unpriced with one source");
    let basic = STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"));
    let tokens: Vec<String> = stand_in.seen(|seen| seen.issued.iter().map(|(token, _)| token.clone()).collect());
    for secret in [CLIENT_ID, CLIENT_SECRET, basic.as_str()].into_iter().chain(tokens.iter().map(String::as_str)) {
        assert_eq!(logs.count(secret), 0, "{secret} reached a log line");
    }
    println!("refused token: venue_token_failed, then venue_feed_down; US 500 unpriced; nothing secret logged");

    // 5. With the feeds stopped, the log replays: every root, signature, and event checks.
    tasks.iter().for_each(AbortHandle::abort);
    stand_in.stop();
    node.submit(Command::Tick).await;
    let replayed = replay_path(&node.dir).expect("the log replays");
    assert!(replayed.commands > 0 && replayed.checkpoints > 0);
}

// ── Helpers ──

/// Runs a CME feed for one symbol against `stand_in` until a second past the end of its first
/// session, failing if two of its quotes are ever more than `LOADED_GAP` apart.
async fn assert_no_gap_across_a_rollover(stand_in: &CmeStandIn, tls: &TlsFront, timing: Timing) {
    let (tx, mut rx) = fake_sequencer();
    let _feed = spawn_cme(stand_in, tls, &[(1, "ES:202612", None)], timing, tx);
    let started = Instant::now();
    let mut switched = None;
    while switched.is_none_or(|at: Instant| at.elapsed() < Duration::from_secs(1)) {
        let received = timeout(LOADED_GAP, rx.recv()).await.expect("a quote within LOADED_GAP").expect("feed up");
        assert!(matches!(received, Request::Submit { cmd: Command::Prices { market: 1, .. }, .. }));
        if switched.is_none() && stand_in.seen(|seen| !seen.ended.is_empty()) {
            switched = Some(Instant::now());
        }
        assert!(started.elapsed() < WAIT * 5, "no rollover in time");
    }
}

/// Runs a CME feed for one symbol against `stand_in`, which keeps expired sessions open, and checks
/// the feed itself ends its first session when the session's token expires.
async fn assert_feed_ends_the_first_session_at_its_expiry(stand_in: &CmeStandIn, tls: &TlsFront, timing: Timing) {
    let (tx, _rx) = fake_sequencer();
    let _feed = spawn_cme(stand_in, tls, &[(1, "ES:202612", None)], timing, tx);
    wait_for(WAIT, || stand_in.seen(|seen| seen.ended.first().cloned())).await;
    let (ended, issued) = stand_in.seen(|seen| (seen.ended[0].clone(), seen.issued[0].clone()));
    assert_eq!(ended.0, issued.0, "the first session ended first");
    // The feed counts the token's life from before it asked, so it ends the session no later.
    let (early, late) = (issued.1.saturating_duration_since(ended.1), ended.1.saturating_duration_since(issued.1));
    let margin = Duration::from_millis(500);
    assert!(early < margin && late < margin, "{early:?} early, {late:?} late");
    assert_eq!(stand_in.seen(|seen| seen.expired_sessions), 0);
}

/// A CME connection's timing: production's pace, pings and delay bound, with a five-second timeout
/// that the dev scenario's twelve-second tokens outlive.
fn cme_timing() -> Timing {
    Timing {
        interval: Duration::from_millis(10),
        timeout: Duration::from_secs(5),
        reconnect_min: Duration::from_millis(200),
        reconnect_max: Duration::from_secs(1),
        stale_after: Duration::from_secs(1),
        ping_interval: Duration::from_millis(500),
        max_delay: Duration::from_secs(2),
    }
}

/// Each venue's US 500 books: CME's half a point above Massive's.
fn prices(venue: Venue) -> Prices {
    let shift: Fixed = if venue == Venue::Cme { "0.5".parse().unwrap() } else { Fixed::ZERO };
    Arc::new(move |symbol, _| {
        let (bid, ask) = match symbol {
            "ESZ6" | "ES:202612" => ("6000", "6001"),
            "ESH7" | "ES:202703" => ("6040", "6041"),
            // A contract CME lists that no symbol names.
            "ES:202709" => ("6080", "6081"),
            _ => return None,
        };
        Some((bid.parse::<Fixed>().unwrap() + shift, ask.parse::<Fixed>().unwrap() + shift))
    })
}

/// Runs a CME feed for `symbols` against `stand_in` over `tls`, sequencing into `tx`.
fn spawn_cme(
    stand_in: &CmeStandIn,
    tls: &TlsFront,
    symbols: &[(MarketId, &str, Option<&str>)],
    timing: Timing,
    tx: tokio::sync::mpsc::Sender<Request>,
) -> JoinHandle<anyhow::Result<()>> {
    let conn = VenueConnection {
        venue: Venue::Cme,
        url: stand_in.stream_url.clone(),
        api_key_env: None,
        api_key_secret: None,
        oauth: None,
        symbols: symbols
            .iter()
            .map(|(market, symbol, contract)| VenueSymbol {
                market: *market,
                symbol: (*symbol).into(),
                contract: contract.map(Into::into),
            })
            .collect(),
    };
    let credentials = Credentials::OAuth(stand_in.client(tls, timing.timeout));
    tokio::spawn(venue::run(conn, credentials, timing, tls.client.clone(), tx))
}

/// Starts a loopback Massive futures feed quoting US 500's contracts, behind TLS from `tls`, and its
/// feed into `node`. Aborting the returned tasks stops them.
async fn start_massive(node: &Node, tls: &TlsFront) -> Vec<AbortHandle> {
    let (listener, plain) = listen().await;
    let (url, front) = tls.relay(&plain).await;
    let server = serve_massive(listener, prices(Venue::Massive));
    let symbols = ["ESZ6", "ESH7"].map(|contract| VenueSymbol {
        market: US500,
        symbol: contract.into(),
        contract: Some(contract.into()),
    });
    let conn = VenueConnection {
        venue: Venue::Massive,
        url: format!("{url}/futures"),
        api_key_env: Some("VENDOR_API_KEY".into()),
        api_key_secret: None,
        oauth: None,
        symbols: symbols.to_vec(),
    };
    let credentials = Credentials::ApiKey(key(KEY));
    let feed = tokio::spawn(venue::run(conn, credentials, fast(WAIT), tls.client.clone(), node.tx.clone()));
    vec![server.abort_handle(), front, feed.abort_handle()]
}

/// US 500 closed for the next hour, listing ESZ6.
fn closed_us500() -> MarketSpec {
    let now = now_ms();
    let schedule = around_the_clock(Some((now - 60_000, now + 3_600_000)));
    MarketSpec { schedule, contracts: vec![contract("ESZ6", now - 86_400_000)], ..listing(US500) }
}

/// The dev genesis's crypto markets and `us500`.
fn genesis(us500: MarketSpec) -> Genesis {
    let mut genesis = dev_config().genesis;
    genesis.markets.retain(|market| [1, 2].contains(&market.id));
    genesis.close_spread_ceilings.retain(|ceiling| [1, 2].contains(&ceiling.market));
    genesis.close_spread_ceilings.push(CloseSpreadCeiling { market: US500, max_close_spread_bps: 30 });
    genesis.markets.push(us500);
    genesis
}

/// A contract rolling in at `from`.
fn contract(name: &str, from: Millis) -> FuturesContract {
    FuturesContract { contract: name.into(), from: DateTime::from_timestamp_millis(from).expect("in range") }
}

/// Buys US 500 and checks it fills at the index of both sources' mids.
async fn assert_fills_at_the_index(node: &Node) {
    let events = node.buy(US500, INDEX).await;
    let index = events.iter().find_map(|event| if let Event::Fill(fill) = event { Some(fill.index) } else { None });
    assert_eq!(index, Some(INDEX.parse().unwrap()), "{events:?}");
}

/// The longest time between two price commands for US 500 carrying a quote from `source`, by their
/// sequenced stamps.
fn longest_gap(applied: &Applied, source: &str) -> Millis {
    let applied = applied.lock().expect("recorder");
    let stamps: Vec<Millis> = applied
        .iter()
        .filter(|(_, cmd, _)| {
            matches!(cmd, Command::Prices { market: US500, quotes } if quotes.iter().any(|quote| quote.source == source))
        })
        .map(|(ts, _, _)| *ts)
        .collect();
    stamps.windows(2).map(|pair| pair[1] - pair[0]).max().unwrap_or(0)
}
