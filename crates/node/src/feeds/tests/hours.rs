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
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::clock::now_ms;
use crate::config::{Genesis, Venue, VenueConnection, VenueSymbol};
use crate::feeds::tests::fake::{accept, bitstamp_book, fast, kraken_book, listen, next_alert, stream};
use crate::feeds::tests::rwa_support::{around_the_clock, commands, events_of, listing, priced, wait_for};
use crate::feeds::tests::scenario::Node;
use crate::feeds::venue::{self, tls_config};
use crate::test_support::{Alert, LOADED_WAIT, capture_alerts, dev_config};

const US500: MarketId = 20;
const AAPL: MarketId = 40;
/// Short enough that a symbol counted as silent while its market is closed would alert in the test.
const TIMEOUT: Duration = Duration::from_millis(400);
/// How long a wait for the feeds or the sequencer lasts. A wait ends the moment its condition holds,
/// so the ceiling only bounds a hang, and a loaded machine is not mistaken for one. Unlike the
/// feeds' own timeout, `TIMEOUT`, it is no part of what the tests check.
const WAIT: Duration = LOADED_WAIT;
/// The commands after the roll that must quote June and never December.
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
    tokio::time::sleep(TIMEOUT * 3).await;
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
    tokio::time::sleep(TIMEOUT * 3).await;
    assert_no_feed_alert(&mut alerts);
    assert!(timeout(Duration::from_millis(100), listener.accept()).await.is_err(), "reconnected despite traffic");
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
    // learn of the roll, and June, now next, is sequenced. The feeds learn of it when their next
    // confirmation is answered, so the check waits for a run of commands after the roll that
    // quotes June and never December, however many commands the confirmation takes to arrive.
    node.relist(futures(now_ms() + 500)).await;
    let rolled = || {
        commands(&applied)
            .iter()
            .position(|(_, _, events)| events.iter().any(|e| matches!(e, Event::MarketRolled { .. })))
    };
    let at = wait_for(WAIT, rolled).await;
    wait_for(WAIT, || {
        let applied = commands(&applied);
        let run = contracts(&applied[applied.len().checked_sub(RUN).filter(|from| *from > at)?..]);
        (!run.contains(&"ESZ6".into()) && run.contains(&"ESM7".into())).then_some(())
    })
    .await;
    assert!(events_of(&applied).iter().all(|e| !matches!(e, Event::PricesRejected { .. })));
    assert_no_feed_alert(&mut alerts);
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
    tokio::time::sleep(TIMEOUT * 3).await;
    assert_no_feed_alert(&mut alerts);
    assert!(priced(&applied, US500).iter().all(|(_, contracts)| !contracts.contains(&Some("ESZ26".into()))));
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

/// A loopback `venue` quoting `names` every 20ms, stamped now, and the URL that reaches it.
async fn listen_and_serve(venue: Venue, names: &[&str]) -> String {
    let (listener, url) = listen().await;
    let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
    tokio::spawn(async move {
        let (ws, _) = accept(&listener).await;
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
    });
    url
}

/// Starts `venue`'s feed for `symbols` into `node`, reading the venue at `url`.
fn start_feed(node: &Node, venue: Venue, symbols: &[(MarketId, &str, Option<&str>)], url: &str) {
    let url = url.to_owned();
    let symbols = symbols
        .iter()
        .map(|(market, symbol, contract)| VenueSymbol {
            market: *market,
            symbol: (*symbol).into(),
            contract: contract.map(Into::into),
        })
        .collect();
    let conn = VenueConnection { venue, url, api_key_env: None, dataset: None, symbols };
    tokio::spawn(venue::run(conn, None, fast(TIMEOUT), tls_config().expect("tls config"), node.tx.clone()));
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
