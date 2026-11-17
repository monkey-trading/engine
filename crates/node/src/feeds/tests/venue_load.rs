//! The venue load dev scenario: BTC-USD and ETH-USD priced from Coinbase and Kraken at production's
//! feed timings, against stand-ins that send what the venues send (Coinbase's full books, 4 MB for
//! BTC-USD and 2 MB for ETH-USD, and both venues' update rates), through links that inject the
//! network's faults, into the sequencer, with the node's monitor watching. Each fault in turn:
//! - a stalled Coinbase connection, its data queueing behind the stall;
//! - dropped Coinbase connections, refused while the link is down;
//! - a slow link, which queues seconds of the full snapshots ahead of the pongs;
//! - ETH-USD going quiet while BTC-USD streams;
//! - ETH-USD served from a source ten minutes behind, as Coinbase served a product on
//!   16 November 2026, for its subscription and the next two;
//! - an outage of both venues;
//! - an outage of trusted time;
//! - with a second connection to each venue, each connection stalled and dropped in turn.
//!
//! While a market is unpriced the node alerts, and once its venues stream again every market is
//! priced within `DEADLINE`, or after an outage of both venues or of trusted time within
//! `timing.reconnect_max` more, the longest a feed waits between attempts. With two connections to
//! each venue, a fault on one at a time never unprices a market. Run by scripts/dev-venue-load.sh,
//! the only place the full-size books are served.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use engine_core::price::compute_index;
use engine_core::types::MarketId;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::{Instant, sleep, timeout};

use crate::clock::now_ms;
use crate::config::{Venue, VenuesSection};
use crate::feeds::tests::coinbase_support::{BTC, CoinbaseVenue, ETH, Product};
use crate::feeds::tests::fake::spawn_feed_into;
use crate::feeds::tests::kraken_support::KrakenVenue;
use crate::feeds::tests::link::{Faults, FaultyLink};
use crate::feeds::tests::toxicity::{boot_with, keep_time, query};
use crate::feeds::venue::Timing;
use crate::metrics::Metrics;
use crate::monitor;
use crate::sequencer::Request;
use crate::tee::EnclaveKey;
use crate::test_support::{Alert, capture_alerts_and_resolutions, dev_config, scratch_dir, trusted_now};

/// How soon every market is priced once its venues stream again.
pub(super) const DEADLINE: Duration = Duration::from_secs(10);

/// How long a market may go unpriced before the monitor alerts, as the production template sets it.
pub(super) const NO_PRICE_AFTER: Duration = Duration::from_secs(5);

/// The production template, whose `[feeds.venues]` timings the feeds run at.
const TEMPLATE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ops/runbooks/production-node.toml");

/// A source ten minutes behind Coinbase's own.
const BEHIND: Duration = Duration::from_secs(600);

pub(super) const MARKETS: [MarketId; 2] = [1, 2];

/// The production template's `[feeds.venues]`: its feeds' timings and connections.
pub(super) fn production_venues() -> VenuesSection {
    let template: toml::Value = toml::from_str(&std::fs::read_to_string(TEMPLATE).expect("the template")).unwrap();
    template["feeds"]["venues"].clone().try_into().expect("its [feeds.venues]")
}

/// Whether `market` has a price, judged at the host clock or the engine's time, whichever is later,
/// so a gap in what is sequenced shows as one.
pub(super) async fn priced(tx: &mpsc::Sender<Request>, market: MarketId) -> bool {
    query(tx, move |engine| {
        let state = &engine.markets()[&market];
        let held = state.quotes_for(None).expect("a spot market");
        compute_index(held, &state.spec.price_rules, now_ms().max(engine.now())).is_ok()
    })
    .await
}

/// Waits for every one of `markets` to be priced, failing `phase` past `within`.
pub(super) async fn wait_priced(tx: &mpsc::Sender<Request>, markets: &[MarketId], within: Duration, phase: &str) {
    let start = Instant::now();
    loop {
        let mut all = true;
        for market in markets {
            all &= priced(tx, *market).await;
        }
        if all {
            println!("{phase}: priced {:?} after {:?}", markets, start.elapsed());
            return;
        }
        assert!(start.elapsed() < within, "{phase}: {markets:?} not priced within {within:?}");
        sleep(Duration::from_millis(100)).await;
    }
}

/// Fails `phase` if any of `MARKETS` goes unpriced over the next `lasting`.
async fn stay_priced(tx: &mpsc::Sender<Request>, lasting: Duration, phase: &str) {
    let start = Instant::now();
    while start.elapsed() < lasting {
        for market in MARKETS {
            assert!(priced(tx, market).await, "{phase}: market {market} unpriced after {:?}", start.elapsed());
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// Waits for an alert, or a resolution (`key`), named `name` for `market`, if one is named, passing
/// over others, failing `phase` past `within`.
async fn expect(
    alerts: &mut mpsc::UnboundedReceiver<Alert>,
    key: &str,
    name: &str,
    market: Option<MarketId>,
    within: Duration,
    phase: &str,
) {
    let wanted = |alert: &Alert| {
        alert.get(key).is_some_and(|got| got == name)
            && market.is_none_or(|market| alert.get("market").is_some_and(|got| *got == market.to_string()))
    };
    let found = async {
        while let Some(alert) = alerts.recv().await {
            if wanted(&alert) {
                return;
            }
        }
    };
    timeout(within, found)
        .await
        .unwrap_or_else(|_| panic!("{phase}: no {key} {name} for {market:?} within {within:?}"));
}

/// Every alert waiting, so the next phase's start with none behind it.
fn drain(alerts: &mut mpsc::UnboundedReceiver<Alert>) -> Vec<Alert> {
    std::iter::from_fn(|| alerts.try_recv().ok()).collect()
}

/// Fails `phase` if `alerts` hold `market_no_price` for `market`.
fn assert_never_unpriced(alerts: &[Alert], market: MarketId, phase: &str) {
    let unpriced = alerts.iter().find(|alert| {
        alert.get("alert").is_some_and(|name| name == "market_no_price")
            && alert.get("market").is_some_and(|got| *got == market.to_string())
    });
    assert!(unpriced.is_none(), "{phase}: market {market} went unpriced: {unpriced:?}");
}

/// Gives the sequencer a trusted time sample now and every 10 s after, as `keep_time` does.
async fn keep_time_now(tx: &mpsc::Sender<Request>) -> AbortHandle {
    tx.send(Request::TimeSample(trusted_now())).await.expect("the sequencer");
    keep_time(tx)
}

#[tokio::test]
#[ignore = "dev scenario, run by scripts/dev-venue-load.sh"]
async fn dev_scenario_venue_load() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let timing = Timing::for_venues(&production_venues());
    let dir = scratch_dir("venue-load");
    let (tx, _sequencer) = boot_with(&dir, &Arc::new(EnclaveKey::generate()), &dev_config().genesis, true);
    let mut time = keep_time(&tx);
    let sources: BTreeMap<MarketId, BTreeSet<String>> =
        MARKETS.into_iter().map(|market| (market, ["coinbase", "kraken"].map(String::from).into())).collect();
    tokio::spawn(monitor::run(tx.clone(), Arc::new(Metrics::new()), NO_PRICE_AFTER, sources));

    // Coinbase's books at the depth Coinbase sends, and Kraken's, each behind a link of its own.
    let coinbase = CoinbaseVenue::start(&[Product { depth: 18_000, ..BTC }, Product { depth: 9_000, ..ETH }]).await;
    let kraken = KrakenVenue::start(&[("BTC/USD", BTC.mid_cents), ("ETH/USD", ETH.mid_cents)]).await;
    let (coinbase_link, kraken_link) = (FaultyLink::start(&coinbase.url).await, FaultyLink::start(&kraken.url).await);
    spawn_feed_into(tx.clone(), Venue::Coinbase, &coinbase_link.url, &[(1, BTC.id), (2, ETH.id)], timing);
    spawn_feed_into(tx.clone(), Venue::Kraken, &kraken_link.url, &[(1, "BTC/USD"), (2, "ETH/USD")], timing);
    wait_priced(&tx, &MARKETS, DEADLINE * 3, "start").await;

    // Faults on Coinbase's connection: each unprices both markets, which alert, and each is priced
    // within the deadline of the fault ending.
    let stalled = Faults { stalled: true, ..Faults::default() };
    let down = Faults { down: true, ..Faults::default() };
    for (phase, fault, lasting) in
        [("stalled", stalled, Duration::from_secs(15)), ("dropped", down, Duration::from_secs(10))]
    {
        drain(&mut alerts);
        coinbase_link.set(fault);
        for market in MARKETS {
            expect(&mut alerts, "alert", "market_no_price", Some(market), lasting, phase).await;
        }
        sleep(lasting.saturating_sub(NO_PRICE_AFTER)).await;
        coinbase_link.set(Faults::default());
        wait_priced(&tx, &MARKETS, DEADLINE, phase).await;
        for market in MARKETS {
            expect(&mut resolved, "alert_resolved", "market_no_price", Some(market), DEADLINE, phase).await;
        }
    }

    // A slow link: a reconnect's full snapshots take six seconds to arrive, the pongs behind them.
    coinbase_link.set(down);
    sleep(Duration::from_millis(200)).await;
    coinbase_link.set(Faults { rate: Some(1_000_000), ..Faults::default() });
    sleep(Duration::from_secs(15)).await;
    coinbase_link.set(Faults::default());
    wait_priced(&tx, &MARKETS, DEADLINE, "slow link").await;

    // ETH goes quiet while BTC streams, then streams again: BTC never goes unpriced.
    drain(&mut alerts);
    coinbase.halt(ETH.id);
    expect(&mut alerts, "alert", "market_no_price", Some(2), DEADLINE * 2, "quiet").await;
    coinbase.resume(ETH.id);
    wait_priced(&tx, &MARKETS, DEADLINE, "quiet").await;
    expect(&mut resolved, "alert_resolved", "market_no_price", Some(2), DEADLINE, "quiet").await;
    assert_never_unpriced(&drain(&mut alerts), 1, "quiet");

    // ETH served ten minutes behind, on this subscription and the next two, as on 16 November: the
    // feed detects it by the books' age and resubscribes until a subscription is current.
    coinbase.set_lag(ETH.id, BEHIND);
    coinbase.behind(ETH.id, &[BEHIND, BEHIND]);
    expect(&mut alerts, "alert", "venue_stale", None, DEADLINE, "stale").await;
    wait_priced(&tx, &MARKETS, DEADLINE * 3, "stale").await;
    assert_never_unpriced(&drain(&mut alerts), 1, "stale");

    // Both venues out: every attempt is refused until they return, and a feed retries at most
    // `reconnect_max` apart.
    drain(&mut alerts);
    coinbase_link.set(down);
    kraken_link.set(down);
    for market in MARKETS {
        expect(&mut alerts, "alert", "market_no_price", Some(market), DEADLINE, "outage").await;
    }
    sleep(Duration::from_secs(15)).await;
    coinbase_link.set(Faults::default());
    kraken_link.set(Faults::default());
    wait_priced(&tx, &MARKETS, timing.reconnect_max + DEADLINE, "outage").await;

    // Trusted time out: past its grace sequencing halts, and once a sample is back it resumes. The
    // feeds back off meanwhile, as nothing they read is sequenced.
    drain(&mut alerts);
    time.abort();
    expect(&mut alerts, "alert", "trusted_time_unavailable", None, Duration::from_secs(90), "time").await;
    time = keep_time_now(&tx).await;
    wait_priced(&tx, &MARKETS, timing.reconnect_max + DEADLINE, "time").await;
    expect(&mut resolved, "alert_resolved", "trusted_time_unavailable", None, DEADLINE, "time").await;

    // A second connection to each venue, as production opens: a fault on one connection at a time
    // leaves every market priced throughout, and the faulted one is current again within the
    // deadline, before its sibling's fault. The first connections are faulted once more at the end,
    // so the second ones' recovery from a drop counts too.
    let (coinbase_second, kraken_second) =
        (FaultyLink::start(&coinbase.url).await, FaultyLink::start(&kraken.url).await);
    spawn_feed_into(tx.clone(), Venue::Coinbase, &coinbase_second.url, &[(1, BTC.id), (2, ETH.id)], timing);
    spawn_feed_into(tx.clone(), Venue::Kraken, &kraken_second.url, &[(1, "BTC/USD"), (2, "ETH/USD")], timing);
    sleep(DEADLINE).await;
    drain(&mut alerts);
    let links =
        |fault| [(&coinbase_link, fault), (&kraken_link, fault), (&coinbase_second, fault), (&kraken_second, fault)];
    let faults = links(stalled).into_iter().chain(links(down)).chain([(&coinbase_link, down), (&kraken_link, down)]);
    for (at, (link, fault)) in faults.enumerate() {
        let phase = format!("redundant {at}, {fault:?}");
        link.set(fault);
        stay_priced(&tx, NO_PRICE_AFTER, &phase).await;
        link.set(Faults::default());
        stay_priced(&tx, DEADLINE, &phase).await;
    }
    let raised = drain(&mut alerts);
    for market in MARKETS {
        assert_never_unpriced(&raised, market, "redundant");
    }
    println!("redundant: priced throughout");
    time.abort();
}
