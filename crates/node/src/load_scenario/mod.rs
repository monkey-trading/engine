//! The load test of a running node: #905's launch book of 100k accounts on the production genesis's
//! 34 markets (`common::launch_book`, every market open around the clock so the run prices them all
//! at any hour), taken over by a node in this process (`node`), whose feeds read loopback stand-ins
//! for every connection the production template opens (`venues`), driven through its TLS API and
//! WebSocket stream by the load generator, a process of its own (`generator`). Each leg runs at a
//! multiple of launch flow (`plan`): launch flow, ten times it, and with `LOAD_RAMP=1` a ramp from
//! twenty times, doubling each step, until every budget has broken, the sequencer's thread is busy
//! `SATURATED` of the time, a request is refused, or the traders reach their rate. Each leg prints order-to-fill and
//! price-to-stamp at p50, p99 and p999, the sequencer's batch time, stream lag, the sequencer's busy
//! share, and the process's CPU and resident memory, judged against #904's launch bar (`report`),
//! and the ramp the first multiple at which each budget broke. Each window prints its batch time at
//! p99 and p999 and the sequencer's busy share beside its breaches, and each window and leg how long the
//! host's tasks waited on storage and for a CPU and the I/O its disks completed, where the host
//! reports them, so a window broken by a disk or a core stall says so. Each window also prints the
//! storage probe's longest block and whether a stall of the runner's own storage excuses it. The
//! screening source answers each call after a remote source's round trip (`SOURCE_DELAY_MS`), and the
//! bar holds the market orders of traders that sent a
//! screened request within its bound, whose decisions the node's refreshes keep fresh, so any of
//! them that waits for the source counts against it. The orders of traders it keeps no decision
//! for, which wait for the source, of those between, and of fresh traders queued behind another
//! trader's wait on their client's one connection print apart. The log the run wrote then replays
//! to its roots from the same book, rebuilt (`sequencer::replay::replay_path_from`): the book is no
//! log, so `engine replay` cannot start from it.
//!
//! The launch and ten-times legs pass only within the bar, on their second run if their first broke it
//! (`rerun`), under the tolerance for a loaded runner that `sequencer::wait_bench::price_wait_budget`
//! grants (`report::tolerate`), also in the windows a stall of the runner's own storage, timed by a
//! probe apart from the node, does not excuse (`storage`). On CI the order-to-fill and
//! stream-lag bars are scaled by the runner's measured speed (`ENGINE_BENCH_SCALE_TO_RUNNER`,
//! `common::rerun`), printed on each leg's lines. They pass only
//! if they measured what they claim (`report::unmeasured`): every request answered 200 with no
//! rejection, so every account stays within its tier's request rate and no request lacks a screening
//! slot, every answer in, a market order filled, every subscriber taking frames to the leg's last
//! commands, and prices stamped.
//!
//! Run by `scripts/dev-load.sh`; its variables size the run: `LOAD_ACCOUNTS` (100,000), `LOAD_TRADERS`
//! (2,000), `LOAD_SUBSCRIBERS` (`api.max_subscribers`), `LOAD_LEG_SECS` (120), `LOAD_CRYPTO_RATE`
//! (every crypto market's price commands a second, in place of each one's measured rate),
//! `LOAD_RWA_MOVES` (each other market's mid moves a second on each of its connections, at most two on
//! CME Group's, whose stand-in reads its prices twice a second) and `LOAD_SOURCE_DELAY_MS` (200).

mod durable;
pub(crate) mod generator;
mod host;
mod node;
pub(crate) mod plan;
mod report;
mod rerun;
mod soak;
mod storage;
mod venues;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use engine_core::types::{MarketId, Millis};
use tokio::sync::{mpsc, oneshot};

use crate::clock::now_ms;
use crate::common::launch_book::{Mix, NamedCounts, Origin, build};
use crate::load_scenario::node::{Boot, Node};
use crate::load_scenario::plan::{Leg, Line, Plan, Ramp};
use crate::load_scenario::report::{BUDGETS, Breach, Resident, Snapshot, judge, print, unmeasured, window_figures};
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
/// Each crypto market's price commands a second from the live venues: BTC-USD's, ETH-USD's and
/// SOL-USD's median seconds in six minutes of `scripts/dev-venue-load.sh --soak` on 18 November 2026,
/// whose busiest seconds were 116, 74 and 30. Each market's four connections share its rate.
pub(crate) const CRYPTO_RATES: [(MarketId, f64); 3] = [(1, 11.0), (2, 15.0), (3, 10.0)];
const CRYPTO_CONNECTIONS: f64 = 4.0;
/// How often each other market's mid moves on each of its two connections [proposal]: no vendor's rate
/// is measured, and CME Group's stand-in quotes twice a second.
pub(crate) const RWA_MOVES: f64 = 2.0;
/// How far before the wall clock the book's clock is, so the node's first stamp comes after its
/// last, which the book's own commands run some 75 seconds past it.
const BOOK_LEAD_MS: Millis = 10 * 60 * 1_000;
/// How long after the wall clock the book's emissions and staking start, and an epoch's length, so no
/// period or epoch closes in a run, whatever date it is run on.
const BOOK_START_AFTER_WALL_MS: Millis = 24 * 60 * 60 * 1_000;
const BOOK_EPOCH_MS: Millis = 24 * 60 * 60 * 1_000;
/// The busy share at which the ramp stops: past it, the sequencer's queue only grows.
const SATURATED: f64 = 0.9;
/// How long the generator may go without a line: a leg, its drain and its subscribers' start.
const LINE_WITHIN: Duration = Duration::from_secs(10 * 60);
/// How long the generator's clients keep trying to reach a node before they give up.
const CONNECT_SECS: u64 = 60;
/// How long every market has to be priced once the feeds start.
const PRICED_WITHIN: Duration = Duration::from_secs(90);
/// How long the screening source takes to answer [#1070]: a remote source's round trip, which a request
/// with no fresh decision waits for.
const SOURCE_DELAY_MS: u64 = 200;

/// One leg's result.
struct Run {
    leg: String,
    multiple: f64,
    breaches: Vec<Breach>,
    windows: Vec<Window>,
    /// What the leg failed to measure (`report::unmeasured`).
    gaps: Vec<String>,
}

/// One window of a leg: the budgets it broke, and why the runner's storage excuses them, if it does
/// (`storage::excuse`).
#[derive(Clone, Debug)]
struct Window {
    breaches: Vec<Breach>,
    excused: Option<String>,
}

/// `name`'s value in the environment, or `default`.
fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().map_or(default, |value| value.parse().unwrap_or_else(|_| panic!("{name}")))
}

#[test]
#[ignore = "load test of a running node, run by scripts/dev-load.sh"]
fn dev_scenario_load() {
    // The runner's scale is measured before the node and the generator load the machine.
    crate::common::rerun::runner_permille();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("the node's runtime");
    runtime.block_on(run());
}

/// How often each market's mid moves: `LOAD_CRYPTO_RATE` sets every crypto market's price commands a
/// second in place of its measured one, and `LOAD_RWA_MOVES` each other market's moves a second.
fn moves() -> Moves {
    let rate = |measured: f64| std::env::var("LOAD_CRYPTO_RATE").map_or(measured, |rate| rate.parse().expect("a rate"));
    let crypto =
        CRYPTO_RATES.iter().map(|(market, measured)| (*market, rate(*measured) / CRYPTO_CONNECTIONS)).collect();
    Moves { crypto, rwa: env("LOAD_RWA_MOVES", RWA_MOVES) }
}

/// The book's mix at the wall clock `now`: its clock `BOOK_LEAD_MS` behind, its emissions and staking
/// starting a day ahead of `now` and not at the deployment's start, which may be in the past.
fn book_mix(now: Millis, accounts: usize) -> Mix {
    let origin = Origin { start_after: BOOK_LEAD_MS + BOOK_START_AFTER_WALL_MS, epoch_ms: BOOK_EPOCH_MS };
    Mix { at: now - BOOK_LEAD_MS, always_open: true, origin: Some(origin), ..Mix::proposal(accounts) }
}

/// The run.
async fn run() {
    let mix = book_mix(now_ms(), env("LOAD_ACCOUNTS", ACCOUNTS));
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

    let source_delay = Duration::from_millis(env("LOAD_SOURCE_DELAY_MS", SOURCE_DELAY_MS));
    let venues = venues::start(moves(), source_delay, false).await;
    let dir = scratch_dir("load");
    let (listener, api) = node::listen().await;
    let node = Node::start(Boot::Book(Box::new(engine)), &dir, listener, &venues, None).await;
    wait_priced(&node.tx).await;

    let legs = vec![
        Leg { name: "launch_flow".into(), multiple: 1.0, secs: env("LOAD_LEG_SECS", LEG_SECS), windows: 0 },
        Leg { name: "ten_times".into(), multiple: 10.0, secs: env("LOAD_LEG_SECS", LEG_SECS), windows: 0 },
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
        screening_bound_ms: u64::try_from(venues.access.max_age.as_millis()).expect("a bound in range"),
        connect_secs: CONNECT_SECS,
    };
    if env("LOAD_RAMP", 0) == 1 {
        plan.ramp = Some(Ramp { max: plan.ceiling(), ..RAMP });
    }
    let runs = drive(&plan, &node).await;

    let (signer, log_dir) = (node.signer, node.log_dir.clone());
    node.stop(true).await.expect("the node drains");
    drop(venues);
    let rebuilt = build(&mix, counts).h.engine;
    let replayed = crate::sequencer::replay::replay_path_from(rebuilt, &log_dir, signer, Default::default())
        .expect("the log replays");
    println!(
        "LOAD replay commands={} checkpoints={}: the run's log replays to its roots from the book",
        replayed.commands, replayed.checkpoints
    );
    conclude(&runs);
}

/// Each market with no price, and why: its refusal, and the age of each quote it holds.
async fn unpriced(tx: &mpsc::Sender<Request>) -> Vec<(MarketId, String)> {
    let (reply, unpriced) = oneshot::channel::<Vec<(MarketId, String)>>();
    let read = move |engine: &engine_core::Engine| {
        let now = now_ms().max(engine.now());
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
    unpriced.await.expect("an answer")
}

/// Waits for every market to be priced from its stand-ins.
async fn wait_priced(tx: &mpsc::Sender<Request>) {
    let started = std::time::Instant::now();
    loop {
        let unpriced = unpriced(tx).await;
        if unpriced.is_empty() {
            println!("LOAD priced every market after {:?}, stress levels {:?}", started.elapsed(), stress(tx).await);
            return;
        }
        assert!(started.elapsed() < PRICED_WITHIN, "markets {unpriced:?} unpriced after {PRICED_WITHIN:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// How many markets stand at each stress level, which sets their spreads: from `V62` the house's loss
/// over the trailing day raises every market to level 2 while the house is behind.
async fn stress(tx: &mpsc::Sender<Request>) -> BTreeMap<usize, usize> {
    let (reply, levels) = oneshot::channel();
    let read = move |engine: &engine_core::Engine| {
        let mut levels = BTreeMap::new();
        engine.markets().values().for_each(|market| *levels.entry(market.stress.level).or_default() += 1);
        let _ = reply.send(levels);
    };
    tx.send(Request::Query(Box::new(read))).await.expect("the sequencer");
    levels.await.expect("an answer")
}

/// The load generator, a child process, and its lines.
struct Generator {
    child: Child,
    /// Where the node's side answers each leg's end.
    answers: ChildStdin,
    lines: mpsc::UnboundedReceiver<Line>,
}

/// Starts the load generator on `plan`, whose lines come back on the channel.
fn spawn_generator(plan: &Plan) -> Generator {
    let mut child = Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", "load_scenario::generator::load_generator", "--ignored", "--nocapture", "--test-threads=1"])
        .env("LOAD_PLAN", serde_json::to_string(plan).expect("a plan encodes"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the load generator starts");
    let answers = child.stdin.take().expect("the generator's input");
    let output = child.stdout.take().expect("the generator's output");
    let (lines_tx, lines) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            // libtest writes the test's name ahead of its first line, on the same line.
            if let Some(at) = line.find("LOADGEN ") {
                let line = &line[at + "LOADGEN ".len()..];
                let _ = lines_tx.send(serde_json::from_str::<Line>(line).expect("a generator line"));
            }
        }
    });
    Generator { child, answers, lines }
}

/// Starts the generator, judges each leg it reports, and stops the ramp once every budget has broken,
/// a leg did not measure what it claims, or the sequencer is saturated.
async fn drive(plan: &Plan, node: &Node) -> Vec<Run> {
    let Generator { mut child, mut answers, mut lines } = spawn_generator(plan);
    let resident = Resident::start();
    let (mut start, mut window) = (Snapshot::take(&node.metrics), Snapshot::take(&node.metrics));
    let mut windows: Vec<Window> = Vec::new();
    let mut runs = Vec::new();
    // The first multiple each budget broke at, over the ramp and the legs before it.
    let mut broken: BTreeMap<(&str, &str), f64> = BTreeMap::new();
    while let Some(line) = tokio::time::timeout(LINE_WITHIN, lines.recv())
        .await
        .unwrap_or_else(|_| panic!("the load generator sent nothing for {LINE_WITHIN:?}"))
    {
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
            Line::Window { leg, index, figures, storage: probe } => {
                let now = Snapshot::take(&node.metrics);
                let delta = now.since(&window);
                let breaches = judge(&figures, &delta);
                let excused = probe.as_ref().and_then(|probe| storage::excuse(&breaches, probe));
                let blocked =
                    probe.map_or_else(|| "n/a".to_owned(), |probe| probe.block.blocked.as_millis().to_string());
                println!(
                    "LOAD leg={leg} window={index} breaches={breaches:?} {} probe_blocked_ms={blocked} \
                     excused={excused:?}",
                    window_figures(&delta)
                );
                window = now;
                windows.push(Window { breaches, excused });
            }
            Line::End { leg, multiple, figures } => {
                let node_figures = Snapshot::take(&node.metrics).since(&start);
                let breaches = judge(&figures, &node_figures);
                print(&leg, multiple, &figures, &node_figures, resident.reset(), &breaches);
                println!("LOAD leg={leg} stress_levels {:?}", stress(&node.tx).await);
                let gaps = unmeasured(&figures, &node_figures);
                if !gaps.is_empty() {
                    println!("LOAD leg={leg} unmeasured {gaps:?}");
                }
                for breach in &breaches {
                    broken.entry((breach.budget, breach.quantile)).or_insert(multiple);
                }
                let exhausted =
                    broken.len() == BUDGETS.len() || !gaps.is_empty() || node_figures.busy_share >= SATURATED;
                let stop = leg.starts_with("ramp_") && exhausted;
                let run = Run { leg, multiple, breaches, windows: std::mem::take(&mut windows), gaps };
                let again = rerun::wanted(&runs, &run);
                if again {
                    println!(
                        "LOAD leg={} rerun: the launch bar broke ({:?}); the leg runs once more on this node, \
                         the bar unchanged",
                        run.leg,
                        rerun::failed(&run)
                    );
                }
                runs.push(run);
                let answer = if again {
                    "again"
                } else if stop {
                    "stop"
                } else {
                    "next"
                };
                writeln!(answers, "{answer}").expect("the generator reads");
            }
        }
    }
    let status = child.wait().expect("the generator ends");
    assert!(status.success(), "the load generator failed: {status}");
    runs
}

/// Prints the first multiple of launch flow each budget broke at, and fails a measured leg outside
/// the bar, or that did not measure what it claims, by its last run: a leg that broke the bar ran once
/// more (`rerun`).
fn conclude(runs: &[Run]) {
    let last = runs.last().map_or(0.0, |run| run.multiple);
    for (budget, quantile) in BUDGETS {
        let first = runs.iter().find(|run| run.breaches.iter().any(|b| (b.budget, b.quantile) == (budget, quantile)));
        match first {
            Some(run) => println!(
                "LOAD first broke {budget} {quantile} at {}x launch flow ({:.0} requests a second)",
                run.multiple,
                plan::requests_per_second(run.multiple)
            ),
            None => println!("LOAD held {budget} {quantile} through {last}x launch flow"),
        }
    }
    let measured = rerun::judged(runs);
    assert_eq!(measured.len(), 2, "both measured legs ran");
    for run in measured {
        assert!(run.gaps.is_empty(), "{} did not measure what it claims: {:?}", run.leg, run.gaps);
        let failed = rerun::failed(run);
        let verdict = if failed.is_empty() { "held".to_owned() } else { format!("broken {failed:?}") };
        println!("LOAD leg={} launch bar: {verdict}", run.leg);
        assert!(failed.is_empty(), "{} broke the launch bar: {failed:?}", run.leg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deployment's start, 2027-01-01, is behind the clock, as it is for a run after it.
    const AFTER_DEPLOYMENT_START_MS: Millis = 1_800_000_000_000;

    #[test]
    fn the_book_builds_at_a_clock_past_the_deployments_start() {
        let counts = NamedCounts { adders: 1, openers: 1, closeouts: 0 };
        let book = build(&book_mix(AFTER_DEPLOYMENT_START_MS, 200), counts);
        let emissions = book.h.engine.emissions().expect("emissions are initialised");
        assert!(emissions.schedule.start_ms > AFTER_DEPLOYMENT_START_MS, "emissions start ahead of the clock");
    }
}
