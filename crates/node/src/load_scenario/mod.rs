//! The load test of a running node: #905's launch book of 100k accounts on the production genesis's
//! 34 markets (`common::launch_book`, every market open around the clock so the run prices them all
//! at any hour), taken over by a node in this process (`node`), whose feeds read loopback stand-ins
//! for every connection the production template opens (`venues`), driven through its TLS API and
//! WebSocket stream by the load generator, a process of its own (`generator`). Each leg runs at a
//! multiple of launch flow (`plan`): launch flow, ten times it, and with `LOAD_RAMP=1` a ramp from
//! twenty times, doubling each step, until a budget breaks. Each leg prints order-to-fill and
//! price-to-stamp at p50, p99 and p999, the sequencer's batch time, stream lag, the sequencer's busy
//! share, and the process's CPU and resident memory, judged against #904's launch bar (`report`),
//! and the ramp the first multiple at which a budget broke. The log the run wrote then replays to its
//! roots from the same book, rebuilt (`sequencer::replay::replay_path_from`): the book is no log, so
//! `engine replay` cannot start from it.
//!
//! The launch and ten-times legs pass only within the bar, under the tolerance for a loaded runner
//! that `sequencer::wait_bench::price_wait_budget` grants (`report::tolerate`), and only with every
//! request answered 200: none refused for its rate, so every account stays within its tier's request
//! rate, none refused for want of a screening slot, and none failed.
//!
//! Run by `scripts/dev-load.sh`; its variables size the run: `LOAD_ACCOUNTS` (100,000), `LOAD_TRADERS`
//! (2,000), `LOAD_SUBSCRIBERS` (`api.max_subscribers`), `LOAD_LEG_SECS` (120), `LOAD_CRYPTO_RATE`
//! (each crypto market's price commands a second) and `LOAD_RWA_MOVES` (each other market's mid moves
//! a second on each of its connections).

mod generator;
mod node;
mod plan;
mod report;
mod venues;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use engine_core::types::{MarketId, Millis};
use tokio::sync::{mpsc, oneshot};

use crate::clock::now_ms;
use crate::common::launch_book::{Mix, NamedCounts, build};
use crate::load_scenario::node::Node;
use crate::load_scenario::plan::{Leg, Line, Plan, Ramp};
use crate::load_scenario::report::{Breach, Resident, Snapshot, judge, print, tolerate};
use crate::load_scenario::venues::Moves;
use crate::sequencer::Request;
use crate::test_support::scratch_dir;

/// The proposal's book.
const ACCOUNTS: usize = 100_000;
/// Traders, and how many share a client address.
const TRADERS: u32 = 2_000;
const ACCOUNTS_PER_CLIENT: u32 = 20;
/// The share of signed subscribers holding a quote subscription [proposal].
const QUOTING_PER_CENT: u32 = 50;
/// Each measured leg's length.
const LEG_SECS: u64 = 120;
/// The windows a leg is judged in, and how many of them a tail may miss its bar in.
const WINDOWS: u32 = 5;
const MISSES: usize = 2;
/// The ramp: from twenty times launch flow, doubling, a minute a step.
const RAMP: Ramp = Ramp { from: 20.0, factor: 2.0, step_secs: 60, max: f64::MAX };
/// Each crypto market's price commands a second from the live venues, the median `scripts/dev-venue-load.sh
/// --soak` printed, which its four connections share.
const CRYPTO_RATE: f64 = 40.0;
const CRYPTO_CONNECTIONS: f64 = 4.0;
/// How often each other market's mid moves on each of its two connections [proposal]: no vendor's rate
/// is measured, and CME Group's stand-in quotes twice a second.
const RWA_MOVES: f64 = 2.0;
/// How far before the wall clock the book's clock is, so the node's first stamp comes after its
/// last, which the book's own commands run some 75 seconds past it.
const BOOK_LEAD_MS: Millis = 10 * 60 * 1_000;
/// How long every market has to be priced once the feeds start.
const PRICED_WITHIN: Duration = Duration::from_secs(90);

/// One leg's result.
struct Run {
    leg: String,
    multiple: f64,
    breaches: Vec<Breach>,
    windows: Vec<Vec<Breach>>,
    refused: u64,
}

/// `name`'s value in the environment, or `default`.
fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().map_or(default, |value| value.parse().unwrap_or_else(|_| panic!("{name}")))
}

#[test]
#[ignore = "load test of a running node, run by scripts/dev-load.sh"]
fn dev_scenario_load() {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("the node's runtime");
    runtime.block_on(run());
}

/// The run.
async fn run() {
    let mix = Mix { at: now_ms() - BOOK_LEAD_MS, always_open: true, ..Mix::proposal(env("LOAD_ACCOUNTS", ACCOUNTS)) };
    let traders = env("LOAD_TRADERS", TRADERS);
    let counts = NamedCounts { adders: 1, openers: traders as usize, closeouts: 0 };
    let mut book = build(&mix, counts);
    println!(
        "LOAD book accounts={} positions={} orders={} tiers={:?} assembled_s={:.0}",
        book.h.engine.accounts().len(),
        book.positions,
        book.orders,
        book.tiers,
        book.assembled_in.as_secs_f64()
    );
    let engine = std::mem::take(&mut book.h.engine);
    drop(book);

    let crypto = env("LOAD_CRYPTO_RATE", CRYPTO_RATE) / CRYPTO_CONNECTIONS;
    let mut venues = venues::start(Moves { crypto, rwa: env("LOAD_RWA_MOVES", RWA_MOVES) }).await;
    let dir = scratch_dir("load");
    let (listener, api) = node::listen().await;
    let node = Node::start(engine, &dir, listener, &mut venues).await;
    wait_priced(&node.tx).await;

    let legs = vec![
        Leg { name: "launch_flow".into(), multiple: 1.0, secs: env("LOAD_LEG_SECS", LEG_SECS) },
        Leg { name: "ten_times".into(), multiple: 10.0, secs: env("LOAD_LEG_SECS", LEG_SECS) },
    ];
    let mut plan = Plan {
        api,
        first_key: 2,
        traders,
        accounts_per_client: ACCOUNTS_PER_CLIENT,
        subscribers: env("LOAD_SUBSCRIBERS", node.api.max_subscribers.get() as u32),
        quoting_per_cent: QUOTING_PER_CENT,
        quote_markets: node.api.quote_max_markets.get(),
        legs,
        ramp: None,
        windows: WINDOWS,
    };
    if env("LOAD_RAMP", 0) == 1 {
        plan.ramp = Some(Ramp { max: plan.ceiling(), ..RAMP });
    }
    let runs = drive(&plan, &node).await;

    let (signer, log_dir) = (node.signer, node.log_dir.clone());
    node.stop().await.expect("the node drains");
    drop(venues);
    let rebuilt = build(&mix, counts).h.engine;
    let replayed = crate::sequencer::replay::replay_path_from(rebuilt, &log_dir, signer).expect("the log replays");
    println!(
        "LOAD replay commands={} checkpoints={}: the run's log replays to its roots from the book",
        replayed.commands, replayed.checkpoints
    );
    conclude(&runs);
}

/// Waits for every market to be priced from its stand-ins.
async fn wait_priced(tx: &mpsc::Sender<Request>) {
    let started = std::time::Instant::now();
    loop {
        let (reply, unpriced) = oneshot::channel::<Vec<(MarketId, String)>>();
        let read = move |engine: &engine_core::Engine| {
            let now = now_ms().max(engine.now());
            // Each market with no price, why, and the age of each quote it holds.
            let unpriced: Vec<(MarketId, String)> = engine
                .markets()
                .iter()
                .filter_map(|(id, market)| {
                    let refused = market.index(now).err()?;
                    let held: Vec<String> = market
                        .quotes
                        .iter()
                        .map(|(source, quote)| format!("{source} {:?} {} ms", quote.contract, now - quote.ts))
                        .collect();
                    Some((*id, format!("{refused:?}, holding {held:?}")))
                })
                .collect();
            let _ = reply.send(unpriced);
        };
        tx.send(Request::Query(Box::new(read))).await.expect("the sequencer");
        let unpriced = unpriced.await.expect("an answer");
        if unpriced.is_empty() {
            println!("LOAD priced every market after {:?}", started.elapsed());
            return;
        }
        assert!(started.elapsed() < PRICED_WITHIN, "markets {unpriced:?} unpriced after {PRICED_WITHIN:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Starts the generator, judges each leg it reports, and stops the ramp at the first that breaks a
/// budget.
async fn drive(plan: &Plan, node: &Node) -> Vec<Run> {
    let mut child = Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", "load_scenario::generator::load_generator", "--ignored", "--nocapture", "--test-threads=1"])
        .env("LOAD_PLAN", serde_json::to_string(plan).expect("a plan encodes"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the load generator starts");
    let mut answers = child.stdin.take().expect("the generator's input");
    let output = child.stdout.take().expect("the generator's output");
    let (lines_tx, mut lines) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if let Some(line) = line.strip_prefix("LOADGEN ") {
                let _ = lines_tx.send(serde_json::from_str::<Line>(line).expect("a generator line"));
            }
        }
    });
    let resident = Resident::start();
    let (mut start, mut window) = (Snapshot::take(&node.metrics), Snapshot::take(&node.metrics));
    let mut windows: Vec<Vec<Breach>> = Vec::new();
    let mut runs = Vec::new();
    while let Some(line) = lines.recv().await {
        match line {
            Line::Ready { traders, subscribers } => {
                println!("LOAD generator ready traders={traders} subscribers={subscribers}")
            }
            Line::Start { leg, multiple } => {
                println!(
                    "LOAD leg={leg} multiple={multiple} requests_per_second={:.1}",
                    plan::requests_per_second(multiple)
                );
                (start, window, windows) = (Snapshot::take(&node.metrics), Snapshot::take(&node.metrics), Vec::new());
                resident.reset();
            }
            Line::Window { leg, index, figures } => {
                let now = Snapshot::take(&node.metrics);
                let breaches = judge(&figures, &now.since(&window));
                println!("LOAD leg={leg} window={index} breaches={breaches:?}");
                window = now;
                windows.push(breaches);
            }
            Line::End { leg, multiple, figures } => {
                let node_figures = Snapshot::take(&node.metrics).since(&start);
                let breaches = judge(&figures, &node_figures);
                print(&leg, multiple, &figures, &node_figures, resident.reset(), &breaches);
                let refused = figures.refused;
                let ramping = leg.starts_with("ramp_");
                let stop = ramping && !breaches.is_empty();
                runs.push(Run { leg, multiple, breaches, windows: std::mem::take(&mut windows), refused });
                writeln!(answers, "{}", if stop { "stop" } else { "next" }).expect("the generator reads");
            }
        }
    }
    let status = child.wait().expect("the generator ends");
    assert!(status.success(), "the load generator failed: {status}");
    runs
}

/// Prints the ramp's breaking point, and fails a measured leg outside the bar or with a request
/// answered with other than 200.
fn conclude(runs: &[Run]) {
    let ramp: Vec<&Run> = runs.iter().filter(|run| run.leg.starts_with("ramp_")).collect();
    match ramp.iter().find(|run| !run.breaches.is_empty()) {
        Some(broke) => println!(
            "LOAD ramp first broke at {}x launch flow ({:.0} requests a second): {:?}",
            broke.multiple,
            plan::requests_per_second(broke.multiple),
            broke.breaches
        ),
        None if !ramp.is_empty() => {
            println!("LOAD ramp held through {}x launch flow", ramp.last().map_or(0.0, |run| run.multiple))
        }
        None => {}
    }
    let measured: Vec<&Run> = runs.iter().filter(|run| !run.leg.starts_with("ramp_")).collect();
    assert_eq!(measured.len(), 2, "both measured legs ran");
    for run in measured {
        assert_eq!(run.refused, 0, "{}: requests answered with other than 200, or failed", run.leg);
        let failed = tolerate(&run.windows, MISSES);
        assert!(failed.is_empty(), "{} broke the launch bar: {failed:?}", run.leg);
    }
}
