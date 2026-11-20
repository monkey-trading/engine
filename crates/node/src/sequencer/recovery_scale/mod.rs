//! Recovery and handover at scale, which `scripts/dev-recovery-scale.sh` runs: a long log of launch
//! flow on #905's launch book (`common::launch_book`, 100k accounts on the production genesis's 34
//! markets), then each way a node comes back on it, timed against #904's recovery bar.
//!
//! `flow` writes the log through a node's sequencer on accelerated sequenced time: `LAUNCH_SECS` of
//! launch flow, then `TEN_TIMES_SECS` of ten times it, each market priced at `load_scenario`'s live
//! rate while its venue is open, printing each day's commands and bytes and failing a day that priced
//! or filled nothing or rejected over 1% of its requests, or a flow that closes no emission period or
//! staking epoch: the book starts both ten minutes after the flow's start, with the staking mirror's
//! frontier reported each minute (`mix`). The node pairs recovery caches as a node with `[snapshots]`
//! does (`paired`): a capture every `CAPTURE_SECS` of sequenced time, the template's interval, its
//! engine encoded by the stats worker's copy and published by the history owner, each capture's hold
//! on the sequencer recorded. The node then takes a cache at a checkpoint and sequences `SUFFIX_SECS`
//! of launch flow past it, an interval by default, before it stops as a hard stop does, with no capture
//! at its last checkpoint (`cache`). On that log:
//! - `engine replay`'s replay, in commands a second and wall time;
//! - a node's restart from the cache to its first command, on a copy of the log and its caches,
//!   through its own recovery and start checks, with the head posted an hour (`POSTING_INTERVAL`)
//!   before the log's end; with the cache's read, the posted head's check, the full pass over the log's
//!   prefix the check replaces and the takeover cut's read of the log's last checkpoint timed apart;
//! - a node's recovery by full replay to its first command, seeding the marks cache as it goes;
//! - a member's replica build, one replay of its copy, and the history index's catch-up over the log;
//! - a tail handover grown under launch flow on the recovered node to about each of `TAILS`, the
//!   largest held 1 MiB under the handover's bound, then the member's start from its replica, which
//!   each switch's window counts. A take that outlasts a predecessor's wait for its confirmation
//!   (`handover::REPLY_TIMEOUT`) and fails only for the missing acknowledgement, its replica at the holder's
//!   final checkpoint, is reported unconfirmed, and no later tail is handed over; any other failure fails the run.
//!
//! The book is engine state, not a log, so the log starts from it, as `load_scenario`'s does: its
//! first command is seq 0, as a log's whose history and catalog start there, each replay starts from
//! the book where a whole log's starts from an empty engine (`Replay::seeded`), and a recovery "from
//! genesis" is a recovery that replays every command of the log. The 30-day full replay is projected
//! from the commands the markets' calendars give 30 days of launch flow and the replay's rate over the
//! launch leg, to its last command. The environment sizes the run: `RECOVERY_ACCOUNTS` (100,000),
//! `RECOVERY_TRADERS` (2,000), `RECOVERY_START_SECS` (how long after the book's clock, Monday 15:00 UTC,
//! the flow starts: 0), `RECOVERY_LAUNCH_SECS` (30 days), `RECOVERY_TEN_TIMES_SECS` (3 days),
//! `RECOVERY_CAPTURE_SECS` (300), `RECOVERY_SUFFIX_SECS` (300), `RECOVERY_TAILS` (bytes, comma-separated)
//! and `RECOVERY_REPLAY_WORKERS` (the threads that check command seals ahead of each replay, the node's
//! included: one per core).

mod bench;
mod cache;
mod flow;
mod handover;
mod orders;
mod paired;
mod report;
mod restart;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_core::Engine;
use engine_core::types::{AccountId, MarketId};

use crate::clock::Clock;
use crate::common::launch_book::{Mix, NamedCounts, Origin, build};
use crate::config::HistorySection;
use crate::history::derived::RowsStore;
use crate::load_scenario::{CRYPTO_RATES, RWA_MOVES};
use crate::log::{ReplayWorkers, SealChecks};
use crate::sequencer::recovery_scale::flow::{Flow, Node, Sequenced};
use crate::sequencer::recovery_scale::handover::{
    Switch, build_replica, copy_log, hand_over, log_bytes, member_config,
};
use crate::sequencer::recovery_scale::orders::Traders;
use crate::sequencer::recovery_scale::paired::{Paired, node_config};
use crate::sequencer::recovery_scale::report::{
    bytes_per_command, conclude, print_captures, print_replay, print_restart, print_switch,
};
use crate::sequencer::recovery_scale::restart::{Recovered, catch_up_history, recover, replay};
use crate::tee::EnclaveKey;
use crate::test_support::scratch_dir;

/// The proposal's book, and the traders the load generator drives.
const ACCOUNTS: usize = 100_000;
const TRADERS: usize = 2_000;
const DAY_SECS: u64 = 86_400;
/// The log [proposal]: 30 days of launch flow, then 3 days of ten times it.
const LAUNCH_SECS: u64 = 30 * DAY_SECS;
const TEN_TIMES_SECS: u64 = 3 * DAY_SECS;
/// The production template's `[snapshots] interval`, and the launch flow sequenced past the newest
/// cache before the node stops: a hard stop just before the next capture.
const CAPTURE_SECS: u64 = 300;
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
/// Emissions' period 0 and staking's epoch 0 start this long after the book's clock, past the few
/// minutes assembling the book moves it on, so the flow crosses their ends. A staking epoch is the
/// deployment's day, or a third of the launch leg if that is shorter, so a short run closes some.
const ORIGIN_AFTER_SECS: u64 = 10 * 60;
const EPOCH_SECS: u64 = DAY_SECS;
/// The daily rotation scenario's budget for a switch (`custody::tests::election_scenario::daily_turns`).
const SWITCH_BUDGET: Duration = Duration::from_secs(1);
/// #904's bar for one command's hold on the sequencer's thread, which a capture's must keep.
const COMMAND_BAR: Duration = Duration::from_millis(500);

/// The book's engine as an encoding, decoded afresh for each replay that starts from it.
pub(super) struct Base {
    encoded: String,
}

impl Base {
    /// The book as a log's first command follows it: its state, with no command applied, so the log
    /// starts at seq 0, where a node's history index and segment catalog start.
    /// Error cases:
    /// - the book does not encode or decode
    fn new(book: &Engine) -> anyhow::Result<Self> {
        let mut fields: BTreeMap<String, Box<serde_json::value::RawValue>> =
            serde_json::from_str(&serde_json::to_string(book)?)?;
        fields.insert("last_seq".into(), serde_json::value::RawValue::from_string("null".into())?);
        Ok(Self { encoded: serde_json::to_string(&fields)? })
    }

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

/// The book's mix for a flow that starts `start_secs` after its clock and runs `launch_secs` of launch
/// flow: #905's proposal with emissions and staking started inside the flow, as a log written from
/// launch day closes a period every ten minutes and an epoch each day.
fn mix(accounts: usize, start_secs: u64, launch_secs: u64) -> Mix {
    let epoch_secs = EPOCH_SECS.min(launch_secs / 3).max(60);
    let origin = Origin {
        start_after: ((start_secs + ORIGIN_AFTER_SECS) * 1_000) as i64,
        epoch_ms: (epoch_secs * 1_000) as i64,
    };
    Mix { origin: Some(origin), ..Mix::proposal(accounts) }
}

/// The run.
async fn run() {
    let accounts = env("RECOVERY_ACCOUNTS", ACCOUNTS);
    let (start_secs, launch_secs) = (env("RECOVERY_START_SECS", 0), env("RECOVERY_LAUNCH_SECS", LAUNCH_SECS));
    let counts = NamedCounts { adders: 1, openers: env("RECOVERY_TRADERS", TRADERS), closeouts: 0 };
    let mut book = build(&mix(accounts, start_secs, launch_secs), counts);
    let engine = std::mem::take(&mut book.h.engine);
    let first_end = engine.emissions().expect("emissions are initialised").schedule.period_end(0);
    assert!(first_end > engine.now() + 1_000, "the first emission period ends before the flow starts");
    let started = Instant::now();
    let base = Base::new(&engine).expect("the book encodes");
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
    drop((book, engine));

    let run = scratch_dir("recovery-scale");
    let copy = run.join("copy");
    let key = Arc::new(EnclaveKey::generate());
    let signer = AccountId(key.address());
    let interval = Duration::from_secs(env("RECOVERY_CAPTURE_SECS", CAPTURE_SECS));
    let cores = std::thread::available_parallelism().expect("the cores this process may use");
    let workers = env("RECOVERY_REPLAY_WORKERS", cores);
    println!("RECOVERY replay_workers={workers}");
    let workers = ReplayWorkers::from(workers);
    let cfg = node_config(&run, interval, (signer, workers));
    let log = cfg.node.log_dir.clone();
    let mut paired = Paired::start(base.engine(), &cfg, key.clone()).expect("the node takes over the book");
    let node = &mut paired.node;
    // The flow can start later than the book, so a short run crosses a session's close.
    node.clock.advance(Duration::from_secs(start_secs));
    let mut flow = Flow::new(traders, rates);
    let bar_commands = flow.scheduled(&node.seq.engine, node.clock.now_ms(), BAR_DAYS * DAY_SECS, 1.0);
    let days = write_log(node, &mut flow, &log);
    let closed = node.closed();
    println!("RECOVERY closes periods={} epochs={}", closed.periods, closed.epochs);
    assert!(closed.periods > 0 && closed.epochs > 0, "the flow closed no emission period or staking epoch");

    // A cache at the node's next checkpoint, then launch flow past it until a hard stop.
    let cached = cache::capture(&mut paired, &cfg).await;
    let holds = paired.node.seq.captures.held.clone();
    print_captures(&holds, &cached);
    flow.run(&mut paired.node, Duration::from_secs(env("RECOVERY_SUFFIX_SECS", CAPTURE_SECS)), 1.0, |_, _, _| {});
    let last = paired.node.checkpoint_now();
    let end_ts = paired.node.seq.engine.now();
    paired.stop().await.expect("the history owner indexes the log");
    let log_total = log_bytes(&log);
    println!("RECOVERY log seq={} bytes={log_total} rss_mib={}", last.seq, rss_mib());

    // `engine replay`'s replay, then the restart from the cache.
    // Each replay is also timed up to the launch leg's last command, the commands the 30-day projection
    // counts being launch flow's alone.
    let launch_last = days.iter().rfind(|day| day.multiple == 1.0).map_or(0, |day| day.seq);
    let replayed = replay(&base, &log, signer, SealChecks::RunEnds, launch_last);
    print_replay("engine_replay", &replayed);
    let restarted = cache::restart(&cfg, key.clone(), (interval, end_ts), &run.join("restart"))
        .await
        .expect("the node restarts from the cache");
    print_restart(&restarted, &cached);
    assert_eq!(restarted.recovered_seq, Some(last.seq), "the restart recovers the whole log past the cache");
    assert_eq!(restarted.last_checkpoint, last.seq, "the takeover cut reads the log's last checkpoint");

    // A node's recovery by full replay, to its first command, which then holds the log.
    let recovered = recover(&base, &log, &run, key.clone(), workers, launch_last).expect("the node recovers the log");
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
    let (mut follower, stats, built) = build_replica(&base, &copy, (signer, workers), (rows.clone(), &member_history));
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
    let (_member, start) = handover::start_from_replica(follower, stats, &copy, key.clone());
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
            let day = Day { multiple, seq, commands: seq - seq_at, bytes: bytes - bytes_at, sequenced };
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

/// One day of the log: its flow, where it ended, the commands and bytes it wrote, and what it sequenced.
struct Day {
    multiple: f64,
    /// The log's last seq at the day's end.
    seq: u64,
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

/// The CPU time the process has used on all its threads, in seconds, from `ps`'s `[[dd-]hh:]mm:ss[.ss]`:
/// to the hundredth on macOS, to the second where `ps` prints whole seconds, as procps does.
fn cpu_secs() -> f64 {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps").args(["-o", "time=", "-p", &pid]).output().expect("ps runs");
    let time = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let (days, clock) = time.split_once('-').map_or((0.0, time.as_str()), |(d, c)| (d.parse().unwrap_or(0.0), c));
    days * 86_400.0 + clock.split(':').fold(0.0, |total, part| total * 60.0 + part.parse::<f64>().unwrap_or(0.0))
}

/// The process's resident memory, in MiB.
fn rss_mib() -> u64 {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output().expect("ps runs");
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().unwrap_or(0) / 1024
}

#[cfg(test)]
mod closes_tests {
    use super::*;

    /// The flow of a run sized as CI's is, 1,200 s of launch flow and then a minute of ten times it, on
    /// a small book priced in BTC-USD alone, closes emission periods and staking epochs.
    #[test]
    fn a_flow_closes_emission_periods_and_staking_epochs() {
        let counts = NamedCounts { adders: 1, openers: 4, closeouts: 0 };
        let mut book = build(&mix(200, 0, 1_200), counts);
        let engine = std::mem::take(&mut book.h.engine);
        let traders = Traders::new(std::mem::take(&mut book.named.openers), &engine);
        let run = scratch_dir("recovery-closes");
        let mut node = Node::unpaired(engine, &run.join("log"), Arc::new(EnclaveKey::generate()));
        let mut flow = Flow::new(traders, BTreeMap::from([(1, 11.0)]));
        flow.run(&mut node, Duration::from_secs(1_200), 1.0, |_, _, _| {});
        flow.run(&mut node, Duration::from_secs(60), 10.0, |_, _, _| {});
        // Ten-minute periods and 400-second epochs, from ten minutes in.
        let closed = node.closed();
        assert!(closed.periods > 0 && closed.epochs > 0, "{closed:?}");
    }
}
