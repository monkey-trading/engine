//! The index's ingest rate on a recorded log's commands, run by hand: `HISTORY_BENCH_LOG` names a log
//! directory whose first command is seq 0, and `HISTORY_BENCH_COMMANDS` how many of its commands to
//! take (20,000), each update of `HISTORY_BENCH_BATCH` commands (the history owner's `INDEX_BATCH`),
//! collecting retired versions every 1,024 commands as the owner does. It prints the elapsed time every
//! 10,000 commands and, at the end, the head's root, which two builds with the same stage and batch sizes
//! given one log must agree on: those sizes set the store's versions and retirement receipts. It derives
//! no rows: a rollover's financing rows, which the owner indexes beside its events, are left out, so a log
//! spanning a rollover reads faster here than the owner indexes it. `ingest_and_collection_at_scale`
//! measures indexing and collection against the index's size, from a store it keeps between runs.

use std::time::Instant;

use super::*;
use crate::history::derived::Taken;
use crate::history::runtime::INDEX_BATCH;
use crate::log::{LogEntry, SegmentedLog, Stop};

mod measure;
use measure::{Passes, Rollover, Step, Written};

#[test]
#[ignore = "a timing run on a recorded log, by hand"]
fn ingest_rate_on_a_recorded_log() {
    let log = std::env::var("HISTORY_BENCH_LOG").expect("HISTORY_BENCH_LOG names a log directory");
    let count = |name: &str, default: usize| std::env::var(name).map_or(default, |n| n.parse().expect("a count"));
    let (limit, batch) = (count("HISTORY_BENCH_COMMANDS", 20_000), count("HISTORY_BENCH_BATCH", INDEX_BATCH));
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

/// Indexing and collection against the index's size, run by hand on a recorded log (`HISTORY_BENCH_LOG`):
/// the store at `HISTORY_SCALE_STORE` grows, or with `HISTORY_SCALE_HEAD` naming a head this bench saved or a
/// history-head certificate resumes, from the log's next command, in updates of `INDEX_BATCH` commands, its
/// retired versions collected every 1,024 commands in up to `HISTORY_SCALE_COLLECTION_CALLS` (32, the owner's)
/// calls, until `HISTORY_SCALE_UNTIL`; then it runs `HISTORY_SCALE_PROBE` (10,240) commands in updates of each
/// size in `HISTORY_SCALE_BATCHES` (8,32,128,512). `HISTORY_SCALE_PASSES` (1) reads the log that many times,
/// each pass's seqs and times moved past the last's, so a short recording feeds a long run, and
/// `HISTORY_SCALE_RETENTION_MS` sets a new store's retention (the dev config's), so expiry starts once that
/// much sequenced time is indexed. As a node's recovery caches do, it pins the head every
/// `HISTORY_SCALE_CACHE_INTERVAL_MS` (300,000) of sequenced time and keeps the newest `HISTORY_SCALE_CACHE_RETAIN`
/// (2) pins, so collection retires only what is older than the oldest of them; a retain of 0 pins nothing.
/// `HISTORY_SCALE_FREE_SHARE` makes collection leave one in that many of the file's pages free for later writes,
/// where the store's own share applies without it. `HISTORY_SCALE_GROW_BATCH` sets the commands an update takes
/// until `HISTORY_SCALE_UNTIL` (`INDEX_BATCH`). A rollover's financing rows, which the log does not hold, are
/// synthesised: `HISTORY_SCALE_ROLLOVER_ROWS` (0) rows, over 100,000 accounts and as many markets as that takes,
/// on the first command of each `HISTORY_SCALE_ROLLOVER_EVERY_MS` (a day) of sequenced time, whose update's time
/// each step prints apart.
/// Every `HISTORY_SCALE_STEP` (20,480) commands, and after each probe, it
/// prints the store's size, the commands a second, indexing's and collection's time a command, what collection
/// retired, the head's proof that a capture's signed head takes, what the updates wrote a command (nodes and
/// their bytes, values and theirs), the events the index holds, its free pages and its bytes an event held,
/// and saves the head beside it. With `HISTORY_SCALE_BREAKDOWN` set it prints, at the end, each table's and
/// index's pages and bytes, and the values and nodes by kind.
#[test]
#[ignore = "indexing and collection against the index's size, by hand"]
fn ingest_and_collection_at_scale() {
    let log = PathBuf::from(std::env::var("HISTORY_BENCH_LOG").expect("HISTORY_BENCH_LOG names a log directory"));
    let path = PathBuf::from(std::env::var("HISTORY_SCALE_STORE").expect("HISTORY_SCALE_STORE names a store"));
    let count = |name: &str, default: usize| std::env::var(name).map_or(default, |n| n.parse().expect("a count"));
    let (until, step, probe) = (
        count("HISTORY_SCALE_UNTIL", 0) as u64,
        count("HISTORY_SCALE_STEP", 20_480),
        count("HISTORY_SCALE_PROBE", 10_240),
    );
    let calls = count("HISTORY_SCALE_COLLECTION_CALLS", 32);
    let batches: Vec<usize> = std::env::var("HISTORY_SCALE_BATCHES")
        .unwrap_or_else(|_| "8,32,128,512".into())
        .split(',')
        .map(|batch| batch.parse().expect("an update size"))
        .collect();
    let saved = path.with_extension("head.json");
    let retention = match std::env::var("HISTORY_SCALE_RETENTION_MS") {
        Ok(ms) => ms.parse().expect("a retention in milliseconds"),
        Err(_) => crate::test_support::dev_config().history.retention_ms().expect("a retention"),
    };
    let mut index = match std::env::var("HISTORY_SCALE_HEAD") {
        Ok(head) => resumed(&path, Path::new(&head)),
        Err(_) => Index::empty(Store::create(&path).expect("a new store"), NAMESPACE, retention).expect("an index"),
    };
    if let Ok(share) = std::env::var("HISTORY_SCALE_FREE_SHARE") {
        index.store.keep_free_pages(share.parse().expect("a share")).expect("the store keeps the share");
    }
    let passes = Passes::of(&log, count("HISTORY_SCALE_PASSES", 1) as u64);
    let from = index.head().last_seq().map_or(0, |seq| seq + 1);
    println!(
        "HISTORY_SCALE from={from} store_mb={} retention_ms={} log_commands={} log_span_ms={} passes={}",
        fs::metadata(&path).expect("the store").len() >> 20,
        index.head().retention_ms,
        passes.commands,
        passes.span,
        passes.count
    );
    let written = Written::open(&path, index.head().root.namespace);
    let (interval, retain) =
        (count("HISTORY_SCALE_CACHE_INTERVAL_MS", 300_000) as i64, count("HISTORY_SCALE_CACHE_RETAIN", 2));
    let mut caches: std::collections::VecDeque<(Millis, crate::authenticated::Lease)> = Default::default();
    let (mut collected, mut pending, mut probes) = (from, Vec::new(), batches.into_iter());
    let (mut probing, mut batch) = (false, count("HISTORY_SCALE_GROW_BATCH", INDEX_BATCH));
    let (rows, every) =
        (count("HISTORY_SCALE_ROLLOVER_ROWS", 0), count("HISTORY_SCALE_ROLLOVER_EVERY_MS", 86_400_000) as i64);
    let rollover = Rollover::new(rows);
    let (mut rolled, mut rolling) = (None, false);
    let (mut step_of, mut started) = (Step::default(), Instant::now());
    passes.read(from, |command| {
        let seq = command.sequenced.seq;
        let period = command.sequenced.ts.div_euclid(every);
        let derived = (rows > 0 && rolled.replace(period).is_some_and(|last| period > last)).then(|| rollover.rows());
        rolling |= derived.is_some();
        pending.push(Taken { command, derived });
        if pending.len() < batch {
            return Ok(());
        }
        let (at, before) = (Instant::now(), index.head().root.version);
        index.ingest_taken(&pending).expect("the index takes the update");
        (step_of.indexing, step_of.commands, step_of.updates) =
            (step_of.indexing + at.elapsed().as_secs_f64(), step_of.commands + pending.len(), step_of.updates + 1);
        if std::mem::take(&mut rolling) {
            step_of.rollovers.push(at.elapsed().as_secs_f64());
        }
        step_of.written += written.between(before, index.head().root.version);
        pending.clear();
        let ts = index.head().last_ts().unwrap_or(0);
        if retain > 0 && caches.back().is_none_or(|(at, _)| ts.div_euclid(interval) > at.div_euclid(interval)) {
            caches.push_back((ts, index.store.lease(&index.head().root).expect("the head pins")));
            if caches.len() > retain {
                caches.pop_front();
            }
        }
        if seq >= collected + 1_024 {
            let at = Instant::now();
            for _ in 0..calls {
                let (root, call) = (index.head().root, Instant::now());
                let report = index.maintenance(crate::authenticated::COMPLETE_RECEIPT_BUDGET).expect("collection");
                step_of.longest_call = step_of.longest_call.max(call.elapsed().as_secs_f64());
                (step_of.deleted, step_of.reclaimed, step_of.queued) =
                    (step_of.deleted + report.deleted, step_of.reclaimed + report.pages_reclaimed, report.queued);
                if report.idle || index.head().root == root {
                    break;
                }
            }
            step_of.collection += at.elapsed().as_secs_f64();
            step_of.longest_turn = step_of.longest_turn.max(at.elapsed().as_secs_f64());
            let at = Instant::now();
            Index::resume(index.store.clone(), index.head()).expect("the head proves");
            step_of.proofs.push(at.elapsed().as_secs_f64());
            collected = seq;
        }
        let label = if probing { format!("probe_batch_{batch}") } else { "grow".to_owned() };
        if step_of.commands < if probing { probe } else { step } {
            return Ok(());
        }
        step_of.print(&label, &index, &path, &written, started.elapsed().as_secs_f64());
        fs::write(&saved, serde_json::to_vec(&index.head()).expect("the head encodes")).expect("the head is saved");
        (step_of, started, probing) = (Step::default(), Instant::now(), probing || seq >= until);
        if probing {
            match probes.next() {
                Some(next) => batch = next,
                None => return Err(Stop.into()),
            }
        }
        Ok(())
    });
    if std::env::var("HISTORY_SCALE_BREAKDOWN").is_ok() {
        written.breakdown();
    }
}

/// The index of the store at `path` resumed at the head in `head`: a head this bench saved, or a
/// history-head certificate's statement, the versions after it discarded.
fn resumed(path: &Path, head: &Path) -> Index {
    let store = Store::open_existing(path).expect("the store");
    store.enable_writes().expect("the store takes writes");
    let text = fs::read_to_string(head).expect("the head");
    let value: serde_json::Value = serde_json::from_str(&text).expect("a head");
    let head: Head = match value.get("payload").and_then(|payload| payload.as_str()) {
        Some(payload) => {
            let statement: serde_json::Value = serde_json::from_str(payload).expect("a head statement");
            serde_json::from_value(statement["head"].clone()).expect("a head")
        }
        None => serde_json::from_value(value).expect("a head"),
    };
    store.discard_after(&head.root).expect("the later versions are discarded");
    Index::resume(store, head).expect("the head resumes")
}
