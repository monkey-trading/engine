//! The index's ingest rate on a recorded log's commands, run by hand: `HISTORY_BENCH_LOG` names a log
//! directory whose first command is seq 0, and `HISTORY_BENCH_COMMANDS` how many of its commands to
//! take (20,000), each update of `HISTORY_BENCH_BATCH` commands (128, the history owner's), collecting
//! retired versions every 1,024 commands as the owner does. It prints the elapsed time every 10,000
//! commands and, at the end, the head's root, which two builds given one log must agree on. It derives no
//! rows: a rollover's financing rows, which the owner indexes beside its events, are left out, so a log
//! spanning a rollover reads faster here than the owner indexes it.

use std::time::Instant;

use super::*;
use crate::history::derived::Taken;
use crate::log::{LogEntry, SegmentedLog, Stop};

#[test]
#[ignore = "a timing run on a recorded log, by hand"]
fn ingest_rate_on_a_recorded_log() {
    let log = std::env::var("HISTORY_BENCH_LOG").expect("HISTORY_BENCH_LOG names a log directory");
    let count = |name: &str, default: usize| std::env::var(name).map_or(default, |n| n.parse().expect("a count"));
    let (limit, batch) = (count("HISTORY_BENCH_COMMANDS", 20_000), count("HISTORY_BENCH_BATCH", 128));
    let mut commands = Vec::new();
    let read = SegmentedLog::open(Path::new(&log), |entry| {
        if let LogEntry::Command(command) = entry {
            commands.push(Taken { command, derived: None });
        }
        if commands.len() == limit { Err(Stop.into()) } else { Ok(()) }
    });
    if let Err(error) = read {
        assert!(error.is::<Stop>(), "the log reads: {error:#}");
    }
    let events = commands.iter().flat_map(|taken| taken.command.events.iter().flatten());
    let events = events.filter(|event| account_of(event).is_some()).count();
    let retention = crate::test_support::dev_config().history.retention_ms().expect("a retention");
    let (mut index, path) = fixture("bench-ingest", retention);
    let (started, mut collected, mut reported) = (Instant::now(), 0, 0);
    for (n, taken) in commands.chunks(batch).enumerate() {
        index.ingest_taken(taken).expect("the index takes the batch");
        if (n + 1) * batch >= reported + 10_000 {
            reported = (n + 1) * batch;
            println!("HISTORY_BENCH_STEP commands={reported} s={:.1}", started.elapsed().as_secs_f64());
        }
        let indexed = index.head().last_seq().unwrap_or(0);
        if indexed >= collected + 1_024 {
            for _ in 0..32 {
                let start = index.head().root;
                let report = index.maintenance(crate::authenticated::COMPLETE_RECEIPT_BUDGET).expect("collection");
                if report.idle || index.head().root == start {
                    break;
                }
            }
            collected = indexed;
        }
    }
    let took = started.elapsed().as_secs_f64();
    let bytes = fs::metadata(&path).expect("the store").len();
    let root = index.head().root;
    println!(
        "HISTORY_BENCH commands={} account_events={events} batch={batch} s={took:.1} commands_per_s={:.0} \
         events_per_s={:.0} store_bytes={bytes} root_version={:?} root_hash={}",
        commands.len(),
        commands.len() as f64 / took,
        events as f64 / took,
        root.version,
        hex::encode(root.hash)
    );
}
