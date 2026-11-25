//! A start from a daily snapshot puts the dashboard record and the marks cache where they stood at its checkpoint,
//! from the history the snapshot carries, and reads no log before it: its answers are those the node gave before.

use std::collections::BTreeMap;
use std::time::Duration;

use engine_core::command::Command;
use engine_core::price::SourceQuote;
use engine_core::types::{MarketId, Millis};
use tokio::sync::{broadcast, mpsc, watch};

use super::*;
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::config::DailySnapshotsSection;
use crate::daily_snapshots::{Listed, read_manifest};
use crate::history::marks::store::{MARKS_FILE, Store};
use crate::history::marks::{MarksPage, Reader, Resolution, Retentions, run};
use crate::log::Manifest;
use crate::sequencer::{Request, SequencerConfig, genesis_commands};
use crate::test_support::{dev_config, sample_around, scratch_dir, sequencer_config, wait_for};

/// Ten minutes before a UTC midnight, on a minute boundary.
const START: Millis = 1_792_540_800_000 - 600_000;
/// The markets the test prices.
const MARKETS: [MarketId; 2] = [1, 2];

/// A node whose log holds a daily snapshot after marks in both markets, started again from that snapshot with
/// the log before it gone, serves the dashboard series and the chart history it served before: the record and the
/// marks cache before the snapshot come from the history it carries, and the replay of the log after it adds the
/// rest. A marks row before the snapshot deleted from the cache after the start fails the read over it.
#[tokio::test]
async fn a_start_from_a_daily_snapshot_answers_as_the_node_did_before_it() {
    let written = Written::run("snapshot-start-history").await;
    let snapshot = written.snapshot.clone();
    let cfg = &written.cfg;
    remove_segments_before(&cfg.node.log_dir, snapshot.seq);

    let marks = marks::startup(cfg, written.key.marks_key(), false).unwrap();
    let metrics = Metrics::new();
    let recovered = recover(cfg, &written.key, None, &marks, None, &metrics).await.unwrap();
    assert_eq!(recovered.log.snapshot_sequence(), Some(snapshot.seq), "the start went on from the snapshot");
    assert_eq!(metrics.full_replays.get(), 0, "the start replayed the whole log");
    assert_eq!(recovered.stats.dump(), written.stats.dump(), "the dashboard record");
    let (_store, reader, ..) = marks.activate().unwrap();
    assert_eq!(pages(&reader).await, written.pages, "the chart history");
    assert!(written.pages.iter().any(|page| page.entries.iter().any(|entry| entry.ts < snapshot.ts)));

    // The host deletes a second inside the chain before the snapshot from the cache the start took.
    let second = written.pages[0].entries.iter().filter(|entry| entry.ts < snapshot.ts).nth(1).unwrap().ts;
    let file = cfg.history.dir.join(MARKS_FILE);
    let deleted = rusqlite::Connection::open(&file)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 0 AND bucket = ?1", [second])
        .unwrap();
    assert_eq!(deleted, 1);
    let read = reader.page(1, Resolution::S1, i64::MIN, i64::MAX, 1_000, 1_000).await;
    assert!(read.is_err(), "the read over the deleted row is served: {read:?}");
}

/// A frozen copy altered after the snapshot was taken is refused at the start, which pages `marks_cache_rebuilt`
/// and starts the log-derived history at the snapshot, while the dashboard record is the one the node kept.
#[tokio::test]
async fn an_altered_frozen_copy_starts_the_chart_history_at_the_snapshot() {
    let written = Written::run("snapshot-start-altered").await;
    let snapshot = written.snapshot.clone();
    let cfg = &written.cfg;
    let offset = crate::log::Offset { seq: snapshot.seq, index: snapshot.index };
    let copy = crate::history::marks::store::frozen_path(&cfg.history.dir, offset);
    let floor: Millis = rusqlite::Connection::open(&copy)
        .unwrap()
        .query_row("SELECT MIN(bucket) FROM marks WHERE market = 1 AND resolution = 0", [], |row| row.get(0))
        .unwrap();
    rusqlite::Connection::open(&copy)
        .unwrap()
        .execute("DELETE FROM marks WHERE market = 1 AND resolution = 0 AND bucket = ?1", [floor])
        .unwrap();

    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let marks = marks::startup(cfg, written.key.marks_key(), false).unwrap();
    let recovered = recover(cfg, &written.key, None, &marks, None, &Metrics::new()).await.unwrap();
    let alert = std::iter::from_fn(|| alerts.try_recv().ok()).find(|alert| alert["alert"] == "marks_cache_rebuilt");
    assert!(alert.is_some(), "the refusal pages");
    assert_eq!(recovered.stats.dump(), written.stats.dump(), "the dashboard record");
    let (_store, reader, ..) = marks.activate().unwrap();
    for page in pages(&reader).await {
        assert!(page.entries.iter().all(|entry| entry.ts >= Resolution::D1.bucket(snapshot.ts)), "{page:?}");
    }
}

// ── Helpers ──

/// A node's log with a daily snapshot after marks in both markets, and what the node answered once it stopped.
struct Written {
    cfg: NodeConfig,
    key: Arc<EnclaveKey>,
    /// The record the node kept, every scan it asked for installed.
    stats: Stats,
    /// Every page of both markets the node's marks cache served.
    pages: Vec<MarksPage>,
    /// The newest snapshot.
    snapshot: Listed,
}

impl Written {
    /// Runs a node with `[daily_snapshots]` and a scan due at every command, as `name`, from ten minutes before a
    /// UTC midnight: genesis, which the first snapshot follows, marks before midnight and after it, the day's
    /// snapshot at the first checkpoint past it, and marks after that, then stops it.
    async fn run(name: &str) -> Self {
        let root = scratch_dir(name);
        let mut cfg = dev_config();
        cfg.node.log_dir = root.join("log");
        cfg.history.dir = root.join("history");
        cfg.daily_snapshots = Some(DailySnapshotsSection { dir: root.join("daily") });
        (cfg.api.stats_interval, cfg.api.stats_light_interval) = (Duration::ZERO, Duration::ZERO);
        let key = Arc::new(EnclaveKey::test_key());
        let clock = Arc::new(TestClock::at(START));
        let (manifest, _) = watch::channel(Manifest::default());
        let seq_cfg = SequencerConfig {
            clock: clock.clone(),
            genesis: genesis_commands(&cfg.genesis),
            daily_snapshots: Some(root.join("daily")),
            ..sequencer_config(&cfg.node.log_dir, manifest)
        };
        let (stream, withdrawals) = (broadcast::channel(16).0, mpsc::unbounded_channel().0);
        let mut seq = Sequencer::recover(seq_cfg, key.clone(), stream, withdrawals).unwrap();
        let stats = Stats::new(&cfg).unwrap();
        seq.track_stats(stats.clone());
        seq.pipeline_log_syncs().unwrap();
        seq.activate_protocol().unwrap();
        seq.genesis(&cfg.genesis).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        let retentions = Retentions::of(cfg.history.marks.as_ref()).unwrap();
        let store = Store::create(&cfg.history.dir.join(MARKS_FILE), retentions, key.marks_key()).unwrap();
        let reader = store.reader();
        let (marks_tx, marks_rx) = mpsc::channel(run::QUEUE);
        seq.track_marks(marks_tx);
        let (tx, rx) = mpsc::channel(64);
        let sequencer = tokio::spawn(seq.run(rx));
        let task = run::run(store, tx.downgrade(), None, BTreeMap::new(), run::Live::new(marks_rx), Default::default());
        let task = tokio::spawn(task);

        let mut mark = 67_000;
        let mut step = async |by: Duration, tx: &mpsc::Sender<Request>| {
            clock.advance(by);
            tx.send(Request::TimeSample(sample_around(clock.as_ref()))).await.unwrap();
            for market in MARKETS {
                mark += 10;
                submit(tx, prices(market, clock.now_ms(), mark)).await;
            }
        };
        let daily = root.join("daily");
        let listed = async |count: usize| {
            wait_for("the daily snapshots", async {
                loop {
                    let listed = read_manifest(&daily).unwrap();
                    if listed.len() == count {
                        return listed;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
        };
        step(Duration::from_secs(61), &tx).await;
        // The writer has published the first day's snapshot, from genesis, before the next day's is due.
        listed(1).await;
        for _ in 0..2 {
            step(Duration::from_secs(61), &tx).await;
        }
        // Past midnight, the first checkpoint takes the day's snapshot, after the marks before it.
        step(Duration::from_secs(600), &tx).await;
        for _ in 0..3 {
            step(Duration::from_secs(2), &tx).await;
        }
        let listed = listed(2).await;
        let snapshot = listed[1].clone();
        assert!(snapshot.ts >= START + 600_000, "the second snapshot is the next day's: {listed:?}");
        drop(tx);
        sequencer.await.unwrap().unwrap();
        task.await.unwrap().unwrap();
        stats.settle();
        let pages = pages(&reader).await;
        assert!(pages.iter().any(|page| page.entries.iter().any(|entry| entry.ts > snapshot.ts)), "marks after it");
        Self { cfg, key, stats, pages, snapshot }
    }
}

/// Every page of the priced markets at every resolution.
async fn pages(reader: &Reader) -> Vec<MarksPage> {
    let mut pages = Vec::new();
    for market in MARKETS {
        for resolution in Resolution::ALL {
            pages.push(reader.page(market, resolution, i64::MIN, i64::MAX, 1_000, 1_000).await.unwrap());
        }
    }
    pages
}

/// Removes the log's segments that end before `seq`, which a start from the snapshot there does not read.
fn remove_segments_before(dir: &std::path::Path, seq: u64) {
    let mut firsts: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            let first = path.file_name()?.to_str()?.strip_suffix(".jsonl")?.parse().ok()?;
            Some((first, path))
        })
        .collect();
    firsts.sort_unstable();
    let ended: Vec<_> = firsts.windows(2).filter(|pair| pair[1].0 <= seq).map(|pair| pair[0].1.clone()).collect();
    assert!(!ended.is_empty(), "the snapshot is in the log's first segment");
    ended.iter().for_each(|path| std::fs::remove_file(path).unwrap());
}

/// Sequences `cmd` and waits until it is flushed and published.
async fn submit(tx: &mpsc::Sender<Request>, cmd: Command) {
    let (reply, applied) = tokio::sync::oneshot::channel();
    tx.send(Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
    applied.await.unwrap();
}

/// Three fresh venue quotes for `market` at `ts`, which the engine's median takes.
fn prices(market: MarketId, ts: Millis, mid: i64) -> Command {
    let quotes = ["mock-a", "mock-b", "mock-c"]
        .iter()
        .map(|source| SourceQuote {
            source: source.to_string(),
            mid: engine_core::Fixed::from_int(mid),
            ts,
            contract: None,
        })
        .collect();
    Command::Prices { market, quotes }
}
