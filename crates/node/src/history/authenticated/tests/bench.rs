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
/// size in `HISTORY_SCALE_BATCHES` (8,32,128,512). Every `HISTORY_SCALE_STEP` (20,480) commands, and after each
/// probe, it prints the store's size, the commands a second, indexing's and collection's time a command, what
/// collection retired, and the head's proof that a capture's signed head takes, and saves the head beside it.
#[test]
#[ignore = "indexing and collection against the index's size, by hand"]
fn ingest_and_collection_at_scale() {
    let log = std::env::var("HISTORY_BENCH_LOG").expect("HISTORY_BENCH_LOG names a log directory");
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
    let retention = crate::test_support::dev_config().history.retention_ms().expect("a retention");
    let mut index = match std::env::var("HISTORY_SCALE_HEAD") {
        Ok(head) => resumed(&path, Path::new(&head)),
        Err(_) => Index::empty(Store::create(&path).expect("a new store"), NAMESPACE, retention).expect("an index"),
    };
    let from = index.head().last_seq().map_or(0, |seq| seq + 1);
    println!("HISTORY_SCALE from={from} store_mb={}", fs::metadata(&path).expect("the store").len() >> 20);
    let (mut collected, mut pending, mut probes) = (from, Vec::new(), batches.into_iter());
    let (mut probing, mut batch) = (false, INDEX_BATCH);
    let (mut step_of, mut started) = (Step::default(), Instant::now());
    let read = SegmentedLog::open(Path::new(&log), |entry| {
        let LogEntry::Command(command) = entry else { return Ok(()) };
        let seq = command.sequenced.seq;
        if seq < from {
            return Ok(());
        }
        pending.push(Taken { command, derived: None });
        if pending.len() < batch {
            return Ok(());
        }
        let at = Instant::now();
        index.ingest_taken(&pending).expect("the index takes the update");
        (step_of.indexing, step_of.commands, step_of.updates) =
            (step_of.indexing + at.elapsed().as_secs_f64(), step_of.commands + pending.len(), step_of.updates + 1);
        pending.clear();
        if seq >= collected + 1_024 {
            let at = Instant::now();
            for _ in 0..calls {
                let root = index.head().root;
                let report = index.maintenance(crate::authenticated::COMPLETE_RECEIPT_BUDGET).expect("collection");
                (step_of.deleted, step_of.reclaimed) =
                    (step_of.deleted + report.deleted, step_of.reclaimed + report.pages_reclaimed);
                if report.idle || index.head().root == root {
                    break;
                }
            }
            step_of.collection += at.elapsed().as_secs_f64();
            let at = Instant::now();
            Index::resume(index.store.clone(), index.head()).expect("the head proves");
            step_of.proofs.push(at.elapsed().as_secs_f64());
            collected = seq;
        }
        let label = if probing { format!("probe_batch_{batch}") } else { "grow".to_owned() };
        if step_of.commands < if probing { probe } else { step } {
            return Ok(());
        }
        step_of.print(&label, &index, &path, started.elapsed().as_secs_f64());
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
    if let Err(error) = read {
        assert!(error.is::<Stop>(), "the log reads: {error:#}");
    }
}

/// What one step of `ingest_and_collection_at_scale` took and did.
#[derive(Default)]
struct Step {
    commands: usize,
    updates: usize,
    indexing: f64,
    collection: f64,
    deleted: usize,
    reclaimed: u64,
    proofs: Vec<f64>,
}

impl Step {
    /// Prints the step's figures, `label`led, for `index` and its store at `path`, over `wall` seconds.
    fn print(&self, label: &str, index: &Index, path: &Path, wall: f64) {
        let per_command = |secs: f64| secs * 1e6 / self.commands as f64;
        let proof_ms = self.proofs.iter().sum::<f64>() * 1e3 / self.proofs.len().max(1) as f64;
        println!(
            "HISTORY_SCALE {label} at={} store_mb={} commands={} commands_per_s={:.0} indexing_us_per_command={:.0} \
             collection_us_per_command={:.0} ms_per_update={:.1} deleted_rows={} reclaimed_pages={} head_proof_ms={proof_ms:.2}",
            index.head().last_seq().unwrap_or(0),
            fs::metadata(path).map_or(0, |metadata| metadata.len()) >> 20,
            self.commands,
            self.commands as f64 / wall,
            per_command(self.indexing),
            per_command(self.collection),
            self.indexing * 1e3 / self.updates.max(1) as f64,
            self.deleted,
            self.reclaimed,
        );
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
