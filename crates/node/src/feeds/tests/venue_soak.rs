//! The manual soak: BTC-USD, ETH-USD and SOL-USD priced from the live venues over every connection
//! the production template opens for them, at its feed timings, with the node's monitor watching, for
//! `SOAK_MINUTES` (30 unless set). Every `DROP_EVERY` it drops connections, as a network fault or a
//! restart does, each reopened at once: each connection alone in turn, then all of them. No market is
//! unpriced for longer than `DEADLINE`, through a drop or between them. Each time one is, it prints
//! for how long and why, with each source's quote age, and it prints each alert as it is raised. At
//! the end it prints, for each market, the price commands a second the sequencer stamped from the
//! venues, at the median second and in the busiest, the rate the load scenario's stand-ins replay
//! (`load_scenario`). It needs the venues, so it runs before each release, by
//! `scripts/dev-venue-load.sh --soak`, and never in CI.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::Command;
use engine_core::market::MarketSpec;
use engine_core::price::compute_index;
use engine_core::types::{MarketId, Millis};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

use crate::clock::now_ms;
use crate::config::{Genesis, VenueConnection, VenuesSection};
use crate::feeds::tests::toxicity::{boot_publishing, keep_time, query};
use crate::feeds::tests::venue_load::{DEADLINE, NO_PRICE_AFTER, production_venues, wait_priced};
use crate::feeds::venue::{self, Credentials, Timing};
use crate::metrics::Metrics;
use crate::monitor;
use crate::sequencer::{Request, StreamBody, StreamMsg};
use crate::tee::EnclaveKey;
use crate::test_support::{capture_alerts_and_resolutions, dev_config, scratch_dir, trusted_now};

/// How often connections are dropped.
const DROP_EVERY: Duration = Duration::from_secs(120);

/// How often each market's price is read.
const POLL: Duration = Duration::from_millis(100);

/// The production template's crypto markets: BTC-USD, ETH-USD and SOL-USD.
const CRYPTO: [MarketId; 3] = [1, 2, 3];

/// Price commands stamped for each market, by the second of sequenced time they were stamped in.
type Stamped = Arc<Mutex<BTreeMap<MarketId, BTreeMap<Millis, u32>>>>;

/// The template's connections that quote `CRYPTO`, the crypto venues' public feeds, each keeping
/// only those markets' symbols.
fn live_connections(venues: VenuesSection) -> Vec<VenueConnection> {
    let quoting = |conn: VenueConnection| {
        let symbols: Vec<_> = conn.symbols.iter().filter(|symbol| CRYPTO.contains(&symbol.market)).cloned().collect();
        (!symbols.is_empty()).then_some(VenueConnection { symbols, ..conn })
    };
    venues.connections.into_iter().filter_map(quoting).collect()
}

/// The dev genesis with SOL-USD listed, under the template's id, as ETH-USD is listed.
fn soak_genesis() -> Genesis {
    let mut genesis = dev_config().genesis;
    let eth = genesis.markets.iter().find(|market| market.id == 2).expect("ETH-USD is listed").clone();
    genesis.markets.push(MarketSpec { id: 3, symbol: "SOL-USD".into(), ..eth });
    genesis.markets.sort_by_key(|market| market.id);
    let ceiling = *genesis.close_spread_ceilings.iter().find(|c| c.market == 2).expect("ETH-USD's ceiling");
    genesis.close_spread_ceilings.push(CloseSpreadCeiling { market: 3, ..ceiling });
    genesis.close_spread_ceilings.sort_by_key(|ceiling| ceiling.market);
    genesis
}

/// Counts each price command the sequencer publishes, by market and the second of its stamp.
fn count_prices(mut stream: broadcast::Receiver<Arc<StreamMsg>>) -> Stamped {
    let stamped = Stamped::default();
    let into = stamped.clone();
    tokio::spawn(async move {
        loop {
            match stream.recv().await {
                Ok(msg) => {
                    if let StreamBody::Command(entry) = &msg.body
                        && let Command::Prices { market, .. } = &entry.sequenced.cmd
                    {
                        let second = entry.sequenced.ts / 1_000;
                        *into.lock().unwrap().entry(*market).or_default().entry(second).or_default() += 1;
                    }
                }
                Err(RecvError::Lagged(missed)) => panic!("the count fell {missed} messages behind the sequencer"),
                Err(RecvError::Closed) => return,
            }
        }
    });
    stamped
}

/// Prints each market's price commands a second over the seconds from `from` to `to`, a second with
/// none counting as 0: the median second and the busiest.
fn print_rates(stamped: &Stamped, from: Millis, to: Millis) {
    let stamped = stamped.lock().unwrap();
    for market in CRYPTO {
        let counted = stamped.get(&market);
        let mut seconds: Vec<u32> = (from / 1_000..to / 1_000)
            .map(|second| counted.and_then(|c| c.get(&second)).copied().unwrap_or(0))
            .collect();
        seconds.sort_unstable();
        let (median, busiest) = (seconds.get(seconds.len() / 2).copied().unwrap_or(0), seconds.last().copied());
        println!(
            "SOAK rate market={market} seconds={} median_per_second={median} busiest_second={}",
            seconds.len(),
            busiest.unwrap_or(0)
        );
    }
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
    let (stream, published) = broadcast::channel(65_536);
    let stamped = count_prices(published);
    let (tx, _sequencer) = boot_publishing(&dir, &Arc::new(EnclaveKey::generate()), &soak_genesis(), true, stream);
    let _time = keep_time(&tx);
    let sources: BTreeMap<MarketId, BTreeSet<String>> = CRYPTO
        .into_iter()
        .map(|market| {
            let quoting = connections.iter().filter(|conn| conn.symbols.iter().any(|symbol| symbol.market == market));
            (market, quoting.map(|conn| conn.venue.name().to_owned()).collect())
        })
        .collect();
    tokio::spawn(monitor::run(
        tx.clone(),
        Arc::new(Metrics::new()),
        NO_PRICE_AFTER,
        sources,
        watch::channel(Some(trusted_now())).1,
    ));
    let mut feeds: Vec<_> = connections.iter().map(|conn| spawn(conn, timing, &tx)).collect();
    wait_priced(&tx, &CRYPTO, DEADLINE * 3, "start").await;

    let (started, end) = (now_ms(), Instant::now() + Duration::from_secs(60 * minutes));
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
        for market in CRYPTO {
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
    print_rates(&stamped, started, now_ms());
}
