//! The sequencer-thread cost of quote reads, for comparison with `hot_path_benchmark`'s 200 µs p99
//! command budget. The engine lists as many markets as the production genesis, all priced, since a
//! quote's risk check walks them. A command waits for the queries gathered in its batch, which
//! reads spaced across the interval keep to about one, so one read's time is the latency a
//! subscription can add, and reads per second times it is the share of the thread they take. A
//! client can force a read every interval by sending its op again, so the benchmark reads at the
//! cap whether or not the log moved. Run it in a quiet CPU window:
//! `cargo test -p engine-node --lib quotes::bench -- --ignored --nocapture`.
//! `command_latency_under_quote_reads` runs the reads and commands through one thread, as the
//! sequencer does, and times the commands from queueing to done.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use engine_core::command::{Command, OrderKind, OrderRequest, Sequenced};
use engine_core::price::SourceQuote;
use engine_core::types::{MarketId, Side};
use engine_core::{Engine, Fixed};

use super::read;
use super::tests::{SMALL, T0, TRADER, engine_with};
use crate::test_support::dev_config;

const SAMPLES: usize = 2000;
/// Markets the production genesis lists.
const LISTED: u32 = 37;

#[test]
#[ignore = "benchmark: run in a quiet CPU window"]
fn quote_read_cost() {
    let cfg = dev_config();
    let (subscribers, interval) = (cfg.api.max_subscribers.get(), cfg.api.quote_interval);
    let mut engine = engine_with(LISTED - dev_config().genesis.markets.len() as u32);
    // Every always-open market the dev genesis lists with a USD or EUR quote, priced, and the trader
    // long in the first.
    let markets = priced_markets(&mut engine);
    let order = OrderRequest {
        account: TRADER,
        client_id: 1,
        market: markets[0],
        side: Side::Buy,
        qty: Fixed::ONE,
        kind: OrderKind::Market { quoted: Fixed::from_int(100), max_slippage_bps: Some(10_000) },
        reduce_only: false,
        auth: None,
    };
    apply(&mut engine, Command::PlaceOrder(order));
    // A funded account that is declined at the first tier top, and one with 150 USD, whose ladder
    // takes the most probes.
    for (name, account) in [("rich", TRADER), ("poor", SMALL)] {
        for count in [1, 2, 4, cfg.api.quote_max_markets.get()] {
            let ids: Vec<MarketId> = markets.iter().take(count).copied().collect();
            let mut times = Vec::with_capacity(SAMPLES);
            for _ in 0..SAMPLES {
                let began = Instant::now();
                let reply = read(&engine, account, &ids);
                times.push(began.elapsed().as_micros());
                assert!(reply.is_ok());
            }
            times.sort_unstable();
            let (p50, p99) = (times[SAMPLES / 2], times[SAMPLES * 99 / 100]);
            let reads_per_s = subscribers as f64 / interval.as_secs_f64();
            println!(
                "QUOTES account={name} markets={} p50_us={p50} p99_us={p99} subscribers={subscribers} reads_per_s={reads_per_s} thread_share_pct={:.1}",
                ids.len(),
                reads_per_s * p50 as f64 / 10_000.0
            );
        }
    }
}

/// Prices the always-open markets, returning their ids in order.
fn priced_markets(engine: &mut Engine) -> Vec<MarketId> {
    let ids: Vec<MarketId> = engine
        .markets()
        .iter()
        .filter(|(_, market)| matches!(market.spec.schedule, engine_core::session::Schedule::AlwaysOpen))
        .map(|(id, _)| *id)
        .collect();
    for id in &ids {
        let quotes = ["a", "b"]
            .map(|source| SourceQuote { source: source.into(), mid: Fixed::from_int(100), ts: T0, contract: None })
            .to_vec();
        apply(engine, Command::Prices { market: *id, quotes });
    }
    ids.into_iter().filter(|id| engine.markets()[id].mark.is_some()).collect()
}

fn apply(engine: &mut Engine, cmd: Command) {
    let seq = engine.last_seq().map_or(0, |seq| seq + 1);
    engine.apply(&Sequenced { seq, ts: T0, cmd }).unwrap();
}

type Job = Box<dyn FnOnce(&mut Engine) + Send>;

/// Commands queued every 2 ms behind `max_subscribers` subscriptions of `quote_max_markets` markets
/// each, read as often as `api.quote_interval` allows, and the same commands alone. The engine is
/// the dev genesis, not `hot_path_benchmark`'s 100k accounts: a quote read touches the account's
/// own entries and the listed markets, so its time does not grow with the account count.
#[test]
#[ignore = "benchmark: run in a quiet CPU window"]
fn command_latency_under_quote_reads() {
    let cfg = dev_config();
    let (subscribers, interval) = (cfg.api.max_subscribers.get(), cfg.api.quote_interval);
    let reads = cfg.api.quote_max_markets.get();
    for subscribed in [false, true] {
        let mut engine = engine_with(LISTED - dev_config().genesis.markets.len() as u32);
        let markets = priced_markets(&mut engine);
        let ids: Vec<MarketId> = markets.iter().cycle().take(reads).copied().collect();
        let (jobs, queue) = mpsc::channel::<Job>();
        let sequencer = std::thread::spawn(move || queue.iter().for_each(|job| job(&mut engine)));
        let began = Instant::now();
        let reader = subscribed.then(|| {
            let jobs = jobs.clone();
            std::thread::spawn(move || {
                let spacing = interval / subscribers as u32;
                let mut due = Instant::now();
                while began.elapsed() < Duration::from_secs(4) {
                    let ids = ids.clone();
                    let _ = jobs.send(Box::new(move |engine: &mut Engine| drop(read(engine, SMALL, &ids))));
                    due += spacing;
                    std::thread::sleep(due.saturating_duration_since(Instant::now()));
                }
            })
        });
        let (done, results) = mpsc::channel();
        let mut latencies = Vec::new();
        for n in 0..1000i64 {
            std::thread::sleep(Duration::from_millis(2));
            let (queued, done) = (Instant::now(), done.clone());
            let quotes = ["a", "b"]
                .map(|source| SourceQuote {
                    source: source.into(),
                    mid: Fixed::from_int(100 + n % 2),
                    ts: T0 + n,
                    contract: None,
                })
                .to_vec();
            let cmd = Command::Prices { market: markets[0], quotes };
            let _ = jobs.send(Box::new(move |engine: &mut Engine| {
                let seq = engine.last_seq().map_or(0, |seq| seq + 1);
                engine.apply(&Sequenced { seq, ts: T0 + n, cmd }).unwrap();
                let _ = done.send(queued.elapsed().as_micros());
            }));
        }
        drop((jobs, done));
        latencies.extend(results.iter());
        if let Some(reader) = reader {
            reader.join().unwrap();
        }
        sequencer.join().unwrap();
        latencies.sort_unstable();
        println!(
            "COMMANDS quote_subscriptions={} p50_us={} p99_us={}",
            if subscribed { subscribers } else { 0 },
            latencies[latencies.len() / 2],
            latencies[latencies.len() * 99 / 100]
        );
    }
}
