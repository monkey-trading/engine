//! Recovery and handover at scale, which `scripts/dev-recovery-scale.sh` runs: a long log of launch
//! flow on #905's launch book (`common::launch_book`, 100k accounts on the production genesis's 34
//! markets), then each way a node comes back on it, timed against #904's recovery bar.
//!
//! `flow` writes the log through a node's sequencer on accelerated sequenced time: `LAUNCH_SECS` of
//! launch flow, then `TEN_TIMES_SECS` of ten times it, each market priced at `load_scenario`'s live
//! rate while its venue is open, printing each day's commands and bytes and failing a day that priced
//! or filled nothing or rejected over 1% of its requests. The node then writes a cache at its
//! checkpoint, as a capture with `[snapshots]` does, and sequences `SUFFIX_SECS` of launch flow past it
//! before it stops. On that log:
//! - `engine replay`'s replay, in commands a second and wall time;
//! - a node's recovery by full replay to its first command, seeding the marks cache as it goes;
//! - its restart from the cache: the cache's read, the commands past it replayed, and the sequencer's
//!   start that the recovery timed. The cache is the engine-only one a capture builds: the history and
//!   catalog stores a node's paired cache also checks are left out;
//! - a member's replica build, one replay of its copy, and the history index's catch-up over the log;
//! - a tail handover grown under launch flow on the recovered node to about each of `TAILS`, the
//!   largest held 1 MiB under the handover's bound, then the member's start from its replica, which
//!   each switch's window counts. A take that outlasts a predecessor's wait for its confirmation
//!   (`handover::REPLY_TIMEOUT`) is reported unconfirmed, and no later tail is handed over.
//!
//! The book is engine state, not a log, so the log starts where the book ends, as `load_scenario`'s
//! does: each replay starts from the book where a whole log's starts from an empty engine
//! (`Replay::seeded`), and a recovery "from genesis" is a recovery that replays every command of the
//! log. The 30-day full replay is projected from the commands the markets' calendars give 30 days of
//! launch flow and the replay's rate. The environment sizes the run: `RECOVERY_ACCOUNTS` (100,000),
//! `RECOVERY_TRADERS` (2,000), `RECOVERY_START_SECS` (how long after the book's clock, Monday 15:00
//! UTC, the flow starts: 0), `RECOVERY_LAUNCH_SECS` (30 days), `RECOVERY_TEN_TIMES_SECS` (3 days),
//! `RECOVERY_SUFFIX_SECS` (an hour), `RECOVERY_TAILS` (bytes, comma-separated) and
//! `RECOVERY_REPLAY_WORKERS` (the threads that check command seals ahead of each replay: one per core).

mod flow;
mod handover;
mod orders;
mod report;
mod restart;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_core::Engine;
use engine_core::types::{AccountId, MarketId};

use crate::clock::Clock;
use crate::common::launch_book::{Mix, NamedCounts, build};
use crate::config::{HistorySection, SnapshotSection};
use crate::history::derived::RowsStore;
use crate::load_scenario::{CRYPTO_RATES, RWA_MOVES};
use crate::log::ReplayWorkers;
use crate::sequencer::recovery_scale::flow::{Flow, Node, Sequenced};
use crate::sequencer::recovery_scale::handover::{
    Switch, build_replica, copy_log, hand_over, log_bytes, member_config,
};
use crate::sequencer::recovery_scale::orders::Traders;
use crate::sequencer::recovery_scale::report::{bytes_per_command, conclude, print_replay, print_switch};
use crate::sequencer::recovery_scale::restart::{Recovered, capture, catch_up_history, recover, replay, restart};
use crate::tee::EnclaveKey;
use crate::test_support::scratch_dir;

/// The proposal's book, and the traders the load generator drives.
const ACCOUNTS: usize = 100_000;
const TRADERS: usize = 2_000;
const DAY_SECS: u64 = 86_400;
/// The log [proposal]: 30 days of launch flow, then 3 days of ten times it.
const LAUNCH_SECS: u64 = 30 * DAY_SECS;
const TEN_TIMES_SECS: u64 = 3 * DAY_SECS;
/// The launch flow sequenced past the newest cache before the node stops.
const SUFFIX_SECS: u64 = 3_600;
/// The tails a handover carries [proposal], the largest `handover::tail::MAX_TAIL_BYTES`, which
/// `handover::hand_over` holds under it.
const TAILS: [u64; 3] = [1 << 20, 64 << 20, 512 << 20];
/// The connections that quote each market but the crypto ones (`load_scenario::venues`).
const RWA_CONNECTIONS: f64 = 2.0;
/// #904's recovery bar: a full replay of `BAR_DAYS` of launch flow, and a restart from the newest
/// cache to its first command.
const BAR_DAYS: u64 = 30;
const FULL_REPLAY_BAR: Duration = Duration::from_secs(30 * 60);
const RESTART_BAR: Duration = Duration::from_secs(2 * 60);
/// The daily rotation scenario's budget for a switch (`custody::tests::election_scenario::daily_turns`).
const SWITCH_BUDGET: Duration = Duration::from_secs(1);

/// The book's engine as an encoding, decoded afresh for each replay that starts from it.
pub(super) struct Base {
    encoded: String,
}

impl Base {
    /// The book, decoded, its transient indexes rebuilt as recovery rebuilds them.
    pub(super) fn engine(&self) -> Engine {
        let mut engine: Engine = serde_json::from_str(&self.encoded).expect("the book decodes");
        engine.restore_indexes();
        engine
    }
}

/// `name`'s value in the environment, or `default`.
fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().map_or(default, |value| value.parse().unwrap_or_else(|_| panic!("{name}")))
}

#[test]
#[ignore = "recovery and handover on the launch book, run by scripts/dev-recovery-scale.sh"]
fn dev_scenario_recovery_scale() {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    runtime.block_on(run());
}

/// The run.
async fn run() {
    let accounts = env("RECOVERY_ACCOUNTS", ACCOUNTS);
    let counts = NamedCounts { adders: 1, openers: env("RECOVERY_TRADERS", TRADERS), closeouts: 0 };
    let mut book = build(&Mix::proposal(accounts), counts);
    let engine = std::mem::take(&mut book.h.engine);
    let started = Instant::now();
    let base = Base { encoded: serde_json::to_string(&engine).expect("the book encodes") };
    println!(
        "RECOVERY book accounts={} positions={} orders={} assembled_s={:.0} encoded_bytes={} encoded_s={:.1} rss_mib={}",
        engine.accounts().len(),
        book.positions,
        book.orders,
        book.assembled_in.as_secs_f64(),
        base.encoded.len(),
        started.elapsed().as_secs_f64(),
        rss_mib()
    );
    let rates = rates(&engine);
    let traders = Traders::new(std::mem::take(&mut book.named.openers), &engine);
    drop(book);

    let run = scratch_dir("recovery-scale");
    let (log, copy) = (run.join("log"), run.join("copy"));
    let key = Arc::new(EnclaveKey::generate());
    let signer = AccountId(key.address());
    let mut node = Node::start(engine, &log, key.clone());
    // The flow can start later than the book, so a short run crosses a session's close.
    node.clock.advance(Duration::from_secs(env("RECOVERY_START_SECS", 0)));
    let mut flow = Flow::new(traders, rates);
    let bar_commands = flow.scheduled(&node.seq.engine, node.clock.now_ms(), BAR_DAYS * DAY_SECS, 1.0);
    let days = write_log(&mut node, &mut flow, &log);

    // A cache at the node's checkpoint, as a capture with `[snapshots]` writes one, then launch flow
    // past it until the node stops.
    let section = SnapshotSection {
        dir: run.join("snapshots"),
        retain: 2.try_into().expect("non-zero"),
        max_bytes: (16usize << 30).try_into().expect("non-zero"),
        trusted_signers: vec![signer],
    };
    let (held, cache_bytes) = capture(&mut node, &section, &key);
    println!(
        "RECOVERY cache seq={} bytes={cache_bytes} capture_held_s={:.2}",
        node.seq.last_seq().unwrap_or(0),
        held.as_secs_f64()
    );
    flow.run(&mut node, Duration::from_secs(env("RECOVERY_SUFFIX_SECS", SUFFIX_SECS)), 1.0, |_, _, _| {});
    let last = node.checkpoint_now();
    let manifest = node.manifest.clone();
    drop(node);
    let log_total = log_bytes(&log);
    println!("RECOVERY log seq={} bytes={log_total} rss_mib={}", last.seq, rss_mib());

    // `engine replay`'s replay, then the restart from the cache.
    let cores = std::thread::available_parallelism().expect("the cores this process may use");
    let workers = env("RECOVERY_REPLAY_WORKERS", cores);
    println!("RECOVERY replay_workers={workers}");
    let workers = ReplayWorkers::from(workers);
    let replayed = replay(&base, &log, signer, workers);
    print_replay("engine_replay", &replayed);
    let restarted =
        restart(&section, &log, manifest, &run, key.clone(), workers).expect("the node restarts from the cache");
    print_replay("cache_suffix", &restarted.suffix);
    println!(
        "RECOVERY restart_from_cache read_s={:.1} suffix_s={:.1} start_s={:.2}",
        restarted.read.as_secs_f64(),
        restarted.suffix.took.as_secs_f64(),
        restarted.start.as_secs_f64()
    );
    assert_eq!(restarted.last_seq, Some(last.seq), "the restart from the cache replays the whole log after it");

    // A node's recovery by full replay, to its first command, which then holds the log.
    let recovered = recover(&base, &log, &run, key.clone(), workers).expect("the node recovers the log");
    print_replay("node_recovery", &recovered.replayed);
    println!(
        "RECOVERY node_recovery start_s={:.2} first_command_s={:.1} rss_mib={}",
        recovered.start.as_secs_f64(),
        recovered.first_command.as_secs_f64(),
        rss_mib()
    );
    let Recovered { node: mut holder, replayed: recovery, history, .. } = recovered;

    // A member's replica of its copy, and the history index's catch-up over the log.
    copy_log(&log, &copy).expect("the member's copy");
    let history_dir = run.join("history");
    let retention = crate::test_support::dev_config().history.retention_ms().expect("a retention");
    let rows = Arc::new(RowsStore::new(&history_dir, retention));
    let member_history =
        HistorySection { dir: run.join("member-history"), ..crate::test_support::dev_config().history };
    let (mut follower, built) = build_replica(&base, &copy, (signer, workers), (rows.clone(), &member_history));
    // The copy and the index hold the log from its first command to the recovered node's first.
    let held = holder.seq.last_seq().unwrap_or(0) + 1 - first_seq(&log);
    println!("RECOVERY replica_build commands={held} s={:.1} rss_mib={}", built.as_secs_f64(), rss_mib());
    let caught_up = catch_up_history(history, &holder, &log, rows).await.expect("the history index catches up");
    println!("RECOVERY history_catch_up commands={held} s={:.1}", caught_up.as_secs_f64());

    // The tail handovers, then the member's start from its replica.
    let peak = flow.peak_commands_per_sec() * bytes_per_command(&days);
    let cfg = member_config(&run);
    let mut switches: Vec<Switch> = Vec::new();
    for (turn, bytes) in (1..).zip(tails()) {
        let switch = hand_over((&mut holder, &mut flow, &key), (&mut follower, &cfg, turn), (bytes, peak), &log).await;
        let confirmed = switch.confirmed;
        switches.push(switch);
        // A predecessor left unconfirmed hands over nothing more, as one that may still run.
        if !confirmed {
            break;
        }
    }
    let (_member, start) = handover::start_from_replica(follower, &copy, key.clone());
    switches.iter().for_each(|switch| print_switch(switch, start));

    conclude(&days, (bar_commands, peak), (&replayed, &recovery), &restarted, (&switches, start));
}

/// The seq of the first command the log in `dir` holds.
fn first_seq(dir: &std::path::Path) -> u64 {
    let manifest = crate::log::Manifest::load(dir).expect("the log's manifest");
    manifest.segments.first().map_or(0, |listed| listed.first_seq)
}

/// The tails a handover carries: `RECOVERY_TAILS`, or `TAILS`.
fn tails() -> Vec<u64> {
    std::env::var("RECOVERY_TAILS").map_or(TAILS.to_vec(), |tails| {
        tails.split(',').map(|bytes| bytes.trim().parse().expect("RECOVERY_TAILS: bytes")).collect()
    })
}

/// Sequences the log's launch flow, then ten times it, printing each day.
fn write_log(node: &mut Node, flow: &mut Flow, log: &std::path::Path) -> Vec<Day> {
    let mut days = Vec::new();
    let (mut seq_at, mut bytes_at) = (node.seq.last_seq().unwrap_or(0), log_bytes(log));
    let legs =
        [(env("RECOVERY_LAUNCH_SECS", LAUNCH_SECS), 1.0), (env("RECOVERY_TEN_TIMES_SECS", TEN_TIMES_SECS), 10.0)];
    assert!(legs[0].0 > 0, "RECOVERY_LAUNCH_SECS: the log needs launch flow to measure");
    for (secs, multiple) in legs {
        flow.run(node, Duration::from_secs(secs), multiple, |node, sequenced, took| {
            let (seq, bytes) = (node.seq.last_seq().unwrap_or(0), log_bytes(log));
            let day = Day { multiple, commands: seq - seq_at, bytes: bytes - bytes_at, sequenced };
            (seq_at, bytes_at) = (seq, bytes);
            println!(
                "RECOVERY day={} flow={multiple}x secs={} commands={} bytes={} prices={} requests={} fills={} rejected={} \
                 wall_s={:.0} rss_mib={}",
                days.len() + 1,
                sequenced.secs,
                day.commands,
                day.bytes,
                sequenced.prices,
                sequenced.requests,
                sequenced.fills,
                sequenced.rejected,
                took.as_secs_f64(),
                rss_mib()
            );
            // The flow the figures stand for: markets priced, requests filling, and almost none refused,
            // as each trader acts only on what it rests and holds.
            assert!(sequenced.prices > 0 && sequenced.fills > 0, "a day of flow priced or filled nothing");
            assert!(sequenced.rejected * 100 <= sequenced.requests, "a day of flow rejected over 1% of its requests");
            days.push(day);
        });
    }
    days
}

/// One day of the log: its flow, the commands and bytes it wrote, and what it sequenced.
struct Day {
    multiple: f64,
    commands: u64,
    bytes: u64,
    sequenced: Sequenced,
}

/// Each market's price commands a second: the crypto markets' live rates, and each other market's
/// moves on each of its connections (`load_scenario`).
fn rates(engine: &Engine) -> BTreeMap<MarketId, f64> {
    let crypto: BTreeMap<MarketId, f64> = CRYPTO_RATES.into_iter().collect();
    engine.markets().keys().map(|id| (*id, crypto.get(id).copied().unwrap_or(RWA_MOVES * RWA_CONNECTIONS))).collect()
}

/// The process's resident memory, in MiB.
fn rss_mib() -> u64 {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output().expect("ps runs");
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().unwrap_or(0) / 1024
}
