//! What the stats record costs on a launch-scale book (#905): 100k accounts at the latest protocol
//! on the production genesis's 34 markets, each holding positions, resting orders up to its tier's
//! limit, a volume book, a toxicity score and session keys (`common::launch_book`, the core's
//! benchmark book). The full and light scans, the engine's encoding for a snapshot, and the process's
//! resident memory with the worker's copy of the engine. Run in a quiet CPU window:
//! `cargo test -p engine-node --lib stats::launch_bench -- --ignored --nocapture --test-threads=1`.

use crate::common::rerun::{hold, rerun_once};
use std::time::Instant;

use engine_core::command::{Command, Sequenced};

use crate::common::launch_book::{Mix, NamedCounts, build};
use crate::stats::bench::{percentile, rss_kib};
use crate::stats::snapshot::{Grid, Layout, scan};

const ACCOUNTS: usize = 100_000;
const SAMPLES: usize = 30;
/// A scan runs on the stats worker, which the sequencer never waits on, so it is held to #904's
/// single-command ceiling alone.
const SCAN_US: u128 = 500_000;

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn launch_book_scans_and_copy() {
    rerun_once("launch_book_scans_and_copy", launch_book_scans_and_copy_measured);
}

fn launch_book_scans_and_copy_measured() {
    let accounts = std::env::var("LAUNCH_BOOK_ACCOUNTS").ok().map_or(ACCOUNTS, |n| n.parse().expect("a count"));
    let rss_book = rss_kib();
    let mut book = build(&Mix::proposal(accounts), NamedCounts { adders: 1, openers: 0, closeouts: 0 });
    let engine = &book.h.engine;
    // Staking is initialised, as at launch, so a full scan walks every account's equity for the excess a
    // close would distribute.
    assert!(book.h.engine.distributable_excess().is_some(), "staking is initialised");
    let rss_built = rss_kib();
    println!(
        "STATS launch_book accounts={accounts} positions={} orders={} resident_mib={} resident_added_mib={}",
        book.positions,
        book.orders,
        rss_built >> 10,
        rss_built.saturating_sub(rss_book) >> 10
    );
    let layout = Layout { closeout: Grid::new(2_000, 100), profit: Grid::new(5_000, 500) };
    for (name, full) in [("scan", true), ("light_scan", false)] {
        let mut scans = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let began = Instant::now();
            let snapshot = scan(engine, 0, engine.now(), layout, full);
            scans.push(began.elapsed().as_micros());
            assert!(snapshot.open_positions >= book.positions as u64, "{} positions", snapshot.open_positions);
        }
        let p99 = percentile(&mut scans.clone(), 99);
        println!(
            "STATS launch_book_{name} accounts={accounts} samples={SAMPLES} p50_us={} p99_us={p99} max_us={}",
            percentile(&mut scans.clone(), 50),
            scans.iter().max().expect("samples")
        );
        hold(&format!("stats_launch_book_{name}"), p99, SCAN_US);
    }
    // What the worker's copy of the engine takes: the process's resident memory before the worker
    // starts, and after its copy is decoded, has followed a command, which builds the indexes the
    // running worker holds, and a scan has run on it. It comes before the encoding below, whose
    // freed buffer the copy would otherwise reuse.
    let before = rss_kib();
    let stats = crate::test_support::stats();
    stats.scan_in_background(&book.h.engine);
    let sequenced = Sequenced {
        seq: book.h.engine.last_seq().map_or(0, |seq| seq + 1),
        ts: book.h.engine.now() + 1,
        cmd: Command::Tick,
    };
    let events = book.h.engine.apply(&sequenced).expect("a tick applies");
    stats.follow(&sequenced);
    // A scan requested of the worker returns only once its copy is decoded and has followed.
    stats.observe(&book.h.engine, sequenced.ts, &events);
    stats.observe(&book.h.engine, sequenced.ts + 61_000, &[]);
    stats.settle();
    let after = rss_kib();
    println!(
        "STATS launch_book_copy accounts={accounts} rss_before_mib={} rss_after_mib={} copy_mib={}",
        before >> 10,
        after >> 10,
        after.saturating_sub(before) >> 10
    );
    let began = Instant::now();
    let encoded = serde_json::to_vec(&book.h.engine).expect("encoding").len();
    println!(
        "STATS launch_book_snapshot_encoding accounts={accounts} us={} bytes={encoded}",
        began.elapsed().as_micros()
    );
}
