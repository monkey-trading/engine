//! The manual soak: BTC-USD and ETH-USD priced from the live venues over every connection the
//! production template opens for them, at its feed timings, with the node's monitor watching, for
//! `SOAK_MINUTES` (30 unless set). Every `DROP_EVERY` it drops connections, as a network fault or a
//! restart does, each reopened at once: each connection alone in turn, then all of them. No market is
//! unpriced for longer than `DEADLINE`, through a drop or between them. Each time one is, it prints
//! for how long and why, with each source's quote age, and it prints each alert as it is raised. It
//! needs the venues, so it runs before each release, by `scripts/dev-venue-load.sh --soak`, and never
//! in CI.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use engine_core::price::compute_index;
use engine_core::types::MarketId;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

use crate::clock::now_ms;
use crate::config::{VenueConnection, VenuesSection};
use crate::feeds::tests::toxicity::{boot_with, keep_time, query};
use crate::feeds::tests::venue_load::{DEADLINE, MARKETS, NO_PRICE_AFTER, production_venues, wait_priced};
use crate::feeds::venue::{self, Credentials, Timing};
use crate::metrics::Metrics;
use crate::monitor;
use crate::sequencer::Request;
use crate::tee::EnclaveKey;
use crate::test_support::{capture_alerts_and_resolutions, dev_config, scratch_dir};

/// How often connections are dropped.
const DROP_EVERY: Duration = Duration::from_secs(120);

/// How often each market's price is read.
const POLL: Duration = Duration::from_millis(100);

/// The template's connections that quote `MARKETS`, the crypto venues' public feeds, each keeping
/// only those markets' symbols.
fn live_connections(venues: VenuesSection) -> Vec<VenueConnection> {
    let quoting = |conn: VenueConnection| {
        let symbols: Vec<_> = conn.symbols.iter().filter(|symbol| MARKETS.contains(&symbol.market)).cloned().collect();
        (!symbols.is_empty()).then_some(VenueConnection { symbols, ..conn })
    };
    venues.connections.into_iter().filter_map(quoting).collect()
}

/// Whether `market` has a price, judged as `venue_load::priced` judges it, or else why not: the
/// engine's refusal and each source's quote age.
async fn standing(tx: &mpsc::Sender<Request>, market: MarketId) -> Result<(), String> {
    query(tx, move |engine| {
        let state = &engine.markets()[&market];
        let held = state.quotes_for(None).expect("a spot market");
        let now = now_ms().max(engine.now());
        compute_index(held, &state.spec.price_rules, now).map(drop).map_err(|refused| {
            let ages: Vec<String> =
                held.iter().map(|(source, quote)| format!("{source} {} ms", now - quote.ts)).collect();
            format!("{refused:?}, quotes aged {}", ages.join(", "))
        })
    })
    .await
}

/// `conn`'s feed, sequencing into `tx`.
fn spawn(conn: &VenueConnection, timing: Timing, tx: &mpsc::Sender<Request>) -> JoinHandle<anyhow::Result<()>> {
    let tls = venue::tls_config().expect("tls config");
    tokio::spawn(venue::run(conn.clone(), Credentials::Public, timing, tls, tx.clone()))
}

#[tokio::test]
#[ignore = "manual soak against the live venues, run by scripts/dev-venue-load.sh --soak"]
async fn soak_live_venues() {
    let minutes: u64 = std::env::var("SOAK_MINUTES").map_or(30, |minutes| minutes.parse().expect("whole minutes"));
    let (_guard, mut alerts, _resolved) = capture_alerts_and_resolutions();
    let venues = production_venues();
    let timing = Timing::for_venues(&venues);
    let connections = live_connections(venues);
    let dir = scratch_dir("venue-soak");
    let (tx, _sequencer) = boot_with(&dir, &Arc::new(EnclaveKey::generate()), &dev_config().genesis, true);
    let _time = keep_time(&tx);
    let sources: BTreeMap<MarketId, BTreeSet<String>> = MARKETS
        .into_iter()
        .map(|market| {
            let quoting = connections.iter().filter(|conn| conn.symbols.iter().any(|symbol| symbol.market == market));
            (market, quoting.map(|conn| conn.venue.name().to_owned()).collect())
        })
        .collect();
    tokio::spawn(monitor::run(tx.clone(), Arc::new(Metrics::new()), NO_PRICE_AFTER, sources));
    let mut feeds: Vec<_> = connections.iter().map(|conn| spawn(conn, timing, &tx)).collect();
    wait_priced(&tx, &MARKETS, DEADLINE * 3, "start").await;

    let end = Instant::now() + Duration::from_secs(60 * minutes);
    let (mut drops, mut next_drop) = (0, Instant::now() + DROP_EVERY);
    let (mut unpriced, mut longest) = (BTreeMap::<MarketId, (Instant, String)>::new(), Duration::ZERO);
    let mut raised: BTreeMap<String, usize> = BTreeMap::new();
    while Instant::now() < end {
        if Instant::now() >= next_drop {
            let dropped: Vec<usize> = match drops % (connections.len() + 1) {
                alone if alone < connections.len() => vec![alone],
                _ => (0..connections.len()).collect(),
            };
            let named: Vec<&str> = dropped.iter().map(|at| connections[*at].url.as_str()).collect();
            for at in dropped {
                feeds[at].abort();
                feeds[at] = spawn(&connections[at], timing, &tx);
            }
            println!("drop {drops}: {named:?}");
            (drops, next_drop) = (drops + 1, next_drop + DROP_EVERY);
        }
        for market in MARKETS {
            match standing(&tx, market).await {
                Ok(()) => {
                    if let Some((since, why)) = unpriced.remove(&market) {
                        let run = since.elapsed();
                        println!("market {market} unpriced for {run:?}: {why}");
                        longest = longest.max(run);
                    }
                }
                Err(why) => {
                    let (since, first) = unpriced.entry(market).or_insert_with(|| (Instant::now(), why));
                    assert!(since.elapsed() <= DEADLINE, "market {market} unpriced for over {DEADLINE:?}: {first}");
                }
            }
        }
        while let Ok(alert) = alerts.try_recv() {
            println!("alert {alert:?}");
            *raised.entry(alert.get("alert").cloned().unwrap_or_default()).or_default() += 1;
        }
        sleep(POLL).await;
    }
    println!("{minutes} minutes, {drops} drops, longest unpriced {longest:?}, alerts {raised:?}");
}
