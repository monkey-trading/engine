//! A drained stop's final capture with the history index lagging at a multi-GB size, run by hand: a node
//! takes over the launch book in `FINAL_CAPTURE_BENCH_DIR`, a new or empty directory, and writes
//! `RECOVERY_LAUNCH_SECS` (two hours) of `REPLAY_BENCH_MULTIPLE` (10) times launch flow on accelerated
//! sequenced time, capturing a cache every `FINAL_CAPTURE_INTERVAL_SECS` (300) of it, as the soak's node does,
//! while its history owner indexes behind the flow. Then it stops as a drained node does: it waits for the
//! capture in flight, captures its final checkpoint and waits for the owner to publish that cache, within
//! `FINAL_CAPTURE_WITHIN_SECS` (120, the soak's drain) in all. It prints the index's size and how far it
//! lagged, and the time to the final cache, or that it did not come in time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_core::types::AccountId;
use tokio::time::timeout;

use crate::common::launch_book::{NamedCounts, build};
use crate::log::ReplayWorkers;
use crate::sequencer::recovery_scale::flow::Flow;
use crate::sequencer::recovery_scale::orders::Traders;
use crate::sequencer::recovery_scale::paired::{Paired, node_config};
use crate::sequencer::recovery_scale::{ACCOUNTS, Base, TRADERS, env, mix, rates};
use crate::snapshot;
use crate::tee::EnclaveKey;

#[test]
#[ignore = "a drained stop's final capture behind a lagging history index, by hand"]
fn final_capture_bench() {
    let dir = PathBuf::from(std::env::var("FINAL_CAPTURE_BENCH_DIR").expect("FINAL_CAPTURE_BENCH_DIR"));
    let empty = std::fs::read_dir(&dir).ok().is_none_or(|mut entries| entries.next().is_none());
    assert!(empty, "FINAL_CAPTURE_BENCH_DIR names a new or empty directory, which the run fills");
    std::fs::create_dir_all(&dir).expect("the bench's directory");
    let (accounts, launch_secs) = (env("RECOVERY_ACCOUNTS", ACCOUNTS), env("RECOVERY_LAUNCH_SECS", 7_200));
    let interval = Duration::from_secs(env("FINAL_CAPTURE_INTERVAL_SECS", 300));
    let within = Duration::from_secs(env("FINAL_CAPTURE_WITHIN_SECS", 120));
    let counts = NamedCounts { adders: 1, openers: env("RECOVERY_TRADERS", TRADERS), closeouts: 0 };
    let mut book = build(&mix(accounts, 0, launch_secs), counts);
    let engine = std::mem::take(&mut book.h.engine);
    let base = Base::new(&engine).expect("the book encodes");
    let rates = rates(&engine);
    let traders = Traders::new(std::mem::take(&mut book.named.openers), &engine);
    drop((book, engine));
    let key = Arc::new(EnclaveKey::test_key());
    let cfg = node_config(&dir, interval, (AccountId(key.address()), ReplayWorkers::available()));
    let section = cfg.snapshots.clone().expect("[snapshots]");
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    runtime.block_on(async {
        let mut paired = Paired::start(base.engine(), &cfg, key).expect("the node takes over the book");
        let mut flow = Flow::new(traders, rates);
        let started = Instant::now();
        let multiple = env("REPLAY_BENCH_MULTIPLE", 10.0);
        flow.run(&mut paired.node, Duration::from_secs(launch_secs), multiple, |_, _, _| {});
        let flushed = paired.node.manifest.borrow().flushed.map_or(0, |at| at.seq);
        // How far the owner lags as the stop begins, at the end of the turn that answers.
        let indexed = paired.sealing.inspect().await.expect("the history owner runs").last_entry;
        let behind = flushed.saturating_sub(indexed.map_or(0, |at| at.seq));
        let index_mb = largest_store(&section.dir) >> 20;
        println!(
            "BENCH flow commands={} wall_s={:.0} index_mb={index_mb} behind_commands={behind}",
            flushed + 1,
            started.elapsed().as_secs_f64()
        );
        // A drained stop: the capture in flight, then the final checkpoint's capture, published.
        let stopping = Instant::now();
        let drained = timeout(within, async {
            paired.sealing.idle().await;
            let in_flight = stopping.elapsed();
            paired.node.seq.capture_next_checkpoint();
            let checkpoint = paired.node.checkpoint_now();
            paired.sealing.idle().await;
            (in_flight, checkpoint.seq)
        })
        .await;
        let took = stopping.elapsed();
        let newest = snapshot::candidates_readonly(&section).expect("the caches").first().map(|path| seq_of(path));
        match drained {
            Ok((in_flight, checkpoint)) => {
                assert_eq!(newest, Some(checkpoint), "the final checkpoint is the newest cache");
                println!(
                    "BENCH final_capture published=true s={:.1} in_flight_s={:.1} checkpoint={checkpoint}",
                    took.as_secs_f64(),
                    in_flight.as_secs_f64()
                );
            }
            Err(_) => {
                println!("BENCH final_capture published=false within_s={} newest_cache={newest:?}", within.as_secs())
            }
        }
        paired.stop().await.expect("the node stops");
    });
}

/// The seq of the cache at `path`, which its file name begins with.
fn seq_of(path: &Path) -> u64 {
    path.file_name().and_then(|name| name.to_str()?.get(..20)?.parse().ok()).expect("a cache file name")
}

/// The size of the largest authenticated store in `dir`: the history index's.
fn largest_store(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .expect("the cache directory")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_str().is_some_and(|name| name.ends_with(".sqlite")))
        .filter_map(|entry| entry.metadata().ok().map(|metadata| metadata.len()))
        .max()
        .unwrap_or(0)
}
