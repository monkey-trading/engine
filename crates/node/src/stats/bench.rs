//! The sequencer-thread cost of the stats record, for comparison with `hot_path_benchmark`'s 200 µs
//! p99 command budget: what each applied command adds (`observe`, which scans no account), and what a
//! scan of every account adds once per `api.stats_interval`, at 100k accounts each holding a
//! position. Run it in a quiet CPU window:
//! `cargo test -p engine-node --lib stats::bench -- --ignored --nocapture`.

use std::time::Instant;

use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};
use engine_core::{Engine, Fixed};

use crate::stats::snapshot::{Grid, Layout, scan};
use crate::stats::tests::{BTC, Rig, T0, TRADER};

const ACCOUNTS: usize = 100_000;
const SAMPLES: usize = 30;

/// A `V29` engine with staking, where an epoch close's excess walks every account's equity, and
/// `ACCOUNTS` accounts each holding one BTC position on 10000 of equity: 100k of notional
/// on a 10% margin, closed out 5% from the mark, half of them short.
fn engine_with_accounts() -> (Rig, Engine) {
    let mut trading = Rig::new();
    trading.trade(T0 + 1_000, TRADER, Side::Buy, 1);
    let held = serde_json::to_value(&trading.engine).expect("fixture encoding")["accounts"][TRADER.to_string()].clone();
    let rig = Rig::staking_at(ProtocolVersion::V29);
    let mut state = serde_json::to_value(&rig.engine).expect("fixture encoding");
    // The trading rig's account, whole: a staking rig's deposits wait on access screening.
    let mut template = held;
    template["balance"] = serde_json::json!("10000");
    let mut accounts = serde_json::Map::new();
    for index in 0..ACCOUNTS {
        let mut address = [0; 20];
        address[12..].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        let key = serde_json::to_value(AccountId(address)).unwrap().as_str().unwrap().to_owned();
        let mut account = template.clone();
        if index % 2 == 1 {
            let position = account["positions"][BTC.to_string()].clone();
            let short = Fixed::from_int(-1);
            account["positions"][BTC.to_string()] = serde_json::json!({ "qty": short, "entry": position["entry"] });
        }
        accounts.insert(key, account);
    }
    state["accounts"] = accounts.into();
    let engine: Engine = serde_json::from_value(state).expect("benchmark fixture");
    (rig, engine)
}

fn percentile(times: &mut [u128], per_cent: usize) -> u128 {
    times.sort_unstable();
    times[(times.len() * per_cent / 100).min(times.len() - 1)]
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn scan_and_observe_cost() {
    let (rig, engine) = engine_with_accounts();
    assert_eq!(engine.accounts().len(), ACCOUNTS);
    assert!(engine.distributable_excess().is_some(), "staking is initialised");
    let layout = Layout { closeout: Grid::new(2_000, 100), profit: Grid::new(5_000, 500) };
    for (name, full) in [("scan", true), ("light_scan", false)] {
        let mut scans = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let began = Instant::now();
            let snapshot = scan(&engine, 0, T0, layout, full);
            scans.push(began.elapsed().as_micros());
            assert!(snapshot.open_positions >= ACCOUNTS as u64);
        }
        println!(
            "STATS {name} accounts={ACCOUNTS} samples={SAMPLES} p50_us={} p99_us={}",
            percentile(&mut scans.clone(), 50),
            percentile(&mut scans, 99)
        );
    }
    // What every command pays: the record's `observe` with no scan due.
    let mut observed = Vec::with_capacity(1000);
    for step in 0..1000 {
        let began = Instant::now();
        rig.stats.observe(&engine, T0 + 2_000 + step, &[]);
        observed.push(began.elapsed().as_micros());
    }
    println!(
        "STATS observe accounts={ACCOUNTS} samples=1000 p50_us={} p99_us={}",
        percentile(&mut observed.clone(), 50),
        percentile(&mut observed, 99)
    );
}

/// What the record costs each command across one scan interval at 100k accounts, with the scan taken
/// on the sequencer's thread and with it handed to the worker: the longest single command and the
/// total, of `follow` and `observe` alone (the engine's own `apply` is not timed). The command
/// that makes the scan due is the one that shares a turn with it.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn command_cost_across_a_scan_interval() {
    use engine_core::command::{Command, Sequenced};
    let (_rig, mut engine) = engine_with_accounts();
    for (name, background) in [("inline", false), ("background", true)] {
        let stats = crate::test_support::stats();
        if background {
            stats.scan_in_background(&engine);
        }
        let mut times = Vec::new();
        let mut at = engine.now() + 1_000;
        for step in 0..=65 {
            let sequenced = Sequenced { seq: engine.last_seq().unwrap() + 1, ts: at, cmd: Command::Tick };
            let events = engine.apply(&sequenced).expect("a tick applies");
            let began = Instant::now();
            stats.follow(&sequenced);
            stats.observe(&engine, at, &events);
            times.push((began.elapsed().as_micros(), step));
            at += 1_000;
        }
        stats.settle();
        let total: u128 = times.iter().map(|(micros, _)| micros).sum();
        let (longest, step) = *times.iter().max().unwrap();
        println!(
            "STATS cycle mode={name} accounts={ACCOUNTS} commands={} longest_us={longest} at_step={step} total_us={total}",
            times.len()
        );
    }
}

/// This process's resident memory in KiB, from `ps`.
fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout).trim().parse().expect("ps prints the resident size")
}

/// What the worker's copy of the engine takes at 100k accounts: the encoded state and the process's
/// resident memory before the worker starts, with its copy decoded, and after a scan has run on it.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn worker_copy_memory() {
    let (_rig, engine) = engine_with_accounts();
    let encoded = serde_json::to_vec(&engine).expect("encoding").len();
    let before = rss_kib();
    let stats = crate::test_support::stats();
    stats.scan_in_background(&engine);
    // A scan requested of the worker returns only once its copy is decoded.
    stats.observe(&engine, engine.now() + 1, &[]);
    stats.observe(&engine, engine.now() + 61_000, &[]);
    stats.settle();
    let after = rss_kib();
    println!(
        "STATS copy accounts={ACCOUNTS} encoded_mib={} rss_before_mib={} rss_after_mib={} copy_mib={}",
        encoded >> 20,
        before >> 10,
        after >> 10,
        after.saturating_sub(before) >> 10
    );
}

/// How far the worker falls behind a sequencer that applies `rate` commands a second, at 100k accounts,
/// with sequenced time running 10 ms a command so the scans come ten times as often as in production: the
/// most commands waiting for it, in commands and milliseconds, the longest `follow` plus `observe`, and
/// what is left to apply when the commands stop. Ticks stand for the commands; each scan is what keeps
/// the worker from them.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn worker_lag_at_the_sequencers_rate() {
    use engine_core::command::{Command, Sequenced};
    let (_rig, mut engine) = engine_with_accounts();
    let stats = crate::test_support::stats();
    stats.scan_in_background(&engine);
    for rate in [1_000u64, 5_000] {
        let (mut costs, mut most_behind, mut sent) = (Vec::new(), 0u64, 0u64);
        let mut at = engine.now() + 10;
        let began = Instant::now();
        while began.elapsed().as_secs() < 14 {
            // Hold to the rate: command `sent` is due `sent / rate` seconds in.
            if began.elapsed().as_micros() < u128::from(sent * 1_000_000 / rate) {
                continue;
            }
            let sequenced = Sequenced { seq: engine.last_seq().unwrap() + 1, ts: at, cmd: Command::Tick };
            let events = engine.apply(&sequenced).expect("a tick applies");
            let cost = Instant::now();
            stats.follow(&sequenced);
            stats.observe(&engine, at, &events);
            costs.push((cost.elapsed().as_micros(), sent));
            most_behind = most_behind.max(stats.behind());
            at += 10;
            sent += 1;
        }
        let left = stats.behind();
        let drained = Instant::now();
        stats.settle();
        let mut sorted = costs.clone();
        sorted.sort_unstable();
        let (p99, p999) = (sorted[sorted.len() * 99 / 100].0, sorted[sorted.len() * 999 / 1000].0);
        let over = sorted.iter().filter(|(micros, _)| *micros > 200).count();
        let worst: Vec<_> = sorted.iter().rev().take(5).collect();
        println!(
            "STATS lag rate={rate}/s commands={sent} most_behind={most_behind} most_behind_ms={} p99_us={p99} p999_us={p999} over_200us={over} worst_us_at_command={worst:?} left_at_stop={left} drained_ms={}",
            most_behind * 1_000 / rate,
            drained.elapsed().as_millis()
        );
    }
}
