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
/// A UTC day.
const DAY_MS: Millis = 86_400_000;
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

/// A start under keys sealed afresh on the same host, as `--new-key` seals them, opens the marks cache under the new
/// key's marks secret and goes on from the snapshot its predecessor's key signed, as the vault's record of that signer
/// admits: the frozen copy written under the earlier key imports on the root the snapshot carries, so the chart
/// history before the snapshot is served as the node served it, with no whole replay and nothing paged. A snapshot
/// written before the root was carried, re-signed without it, starts the chart history at the snapshot and pages
/// `marks_cache_rebuilt`, as before.
#[tokio::test]
async fn a_start_under_new_keys_keeps_the_chart_history_before_the_snapshot() {
    let written = Written::run("snapshot-start-new-key").await;
    let (snapshot, cfg) = (written.snapshot.clone(), &written.cfg);
    let sealed = EnclaveKey::generate();
    assert_ne!(sealed.marks_key(), written.key.marks_key());

    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let marks = marks::startup(cfg, sealed.marks_key(), false).unwrap();
    let metrics = Metrics::new();
    let recovered = recover(cfg, &written.key, None, &marks, None, &metrics).await.unwrap();
    assert_eq!(recovered.log.snapshot_sequence(), Some(snapshot.seq), "the start went on from the snapshot");
    assert_eq!(metrics.full_replays.get(), 0, "the start replayed the whole log");
    let (store, reader, ..) = marks.activate().unwrap();
    assert_eq!(pages(&reader).await, written.pages, "the chart history");
    assert!(written.pages.iter().any(|page| page.entries.iter().any(|entry| entry.ts < snapshot.ts)));
    let rebuilt = |alerts: &mut mpsc::UnboundedReceiver<crate::test_support::Alert>| {
        std::iter::from_fn(|| alerts.try_recv().ok()).filter(|alert| alert["alert"] == "marks_cache_rebuilt").count()
    };
    assert_eq!(rebuilt(&mut alerts), 0, "the start paged a rebuilt cache");
    drop((store, reader));

    let file = cfg.daily_snapshots.as_ref().unwrap().dir.join(&snapshot.file);
    resign(&file, cfg, |_, history| history.marks.as_mut().unwrap().root = None);
    let marks = marks::startup(cfg, sealed.marks_key(), false).unwrap();
    let recovered = recover(cfg, &written.key, None, &marks, None, &Metrics::new()).await.unwrap();
    assert_eq!(recovered.log.snapshot_sequence(), Some(snapshot.seq), "the start went on from the snapshot");
    assert_eq!(rebuilt(&mut alerts), 1, "the copy an earlier snapshot names is taken under new keys");
    let (_store, reader, ..) = marks.activate().unwrap();
    for page in pages(&reader).await {
        assert!(page.entries.iter().all(|entry| entry.ts >= Resolution::D1.bucket(snapshot.ts)), "{page:?}");
    }
}

/// A daily snapshot its signer signed but whose replay state does not stand for its checkpoint, as a writer's fault
/// would sign it, is paged as Warning `snapshot_start_refused` and the start replays the whole log, re-deriving the
/// chart history and the dashboard record the snapshot's restore had put in place.
#[tokio::test]
async fn a_snapshot_the_start_cannot_go_on_from_gives_way_to_a_whole_replay() {
    let written = Written::run("snapshot-start-refused").await;
    let cfg = &written.cfg;
    resign(&cfg.daily_snapshots.as_ref().unwrap().dir.join(&written.snapshot.file), cfg, |carried, _| {
        carried["links"].as_array_mut().unwrap().pop();
    });

    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let marks = marks::startup(cfg, written.key.marks_key(), false).unwrap();
    let metrics = Metrics::new();
    let recovered = recover(cfg, &written.key, None, &marks, None, &metrics).await.unwrap();
    let refused =
        std::iter::from_fn(|| alerts.try_recv().ok()).find(|alert| alert["alert"] == "snapshot_start_refused");
    assert!(refused.is_some(), "the refusal pages");
    assert_eq!(recovered.log.snapshot_sequence(), None, "the start went on from the refused snapshot");
    assert_eq!(metrics.full_replays.get(), 1, "the whole replay is counted");
    assert_eq!(recovered.stats.dump(), written.stats.dump(), "the dashboard record");
    let (_store, reader, ..) = marks.activate().unwrap();
    assert_eq!(pages(&reader).await, written.pages, "the chart history");
}

/// With `[snapshots]` and no cache to take, the start rebuilds the account history index from the whole log, which it
/// then replays whole, not from the daily snapshot, so the index takes only what the replay verified; its dashboard
/// record answers as the node's did.
#[tokio::test]
async fn a_paired_start_with_no_cache_replays_the_whole_log() {
    let mut written = Written::run("snapshot-start-paired").await;
    let root = written.cfg.node.log_dir.parent().unwrap().to_owned();
    written.cfg.snapshots = Some(crate::config::SnapshotSection {
        dir: root.join("snapshots"),
        retain: 4.try_into().unwrap(),
        interval: Duration::ZERO,
        max_bytes: (16 * 1024 * 1024).try_into().unwrap(),
        trusted_signers: Vec::new(),
    });
    let cfg = &written.cfg;
    let marks = marks::startup(cfg, written.key.marks_key(), false).unwrap();
    let recovered = recover(cfg, &written.key, None, &marks, None, &Metrics::new()).await.unwrap();
    assert_eq!(recovered.log.snapshot_sequence(), None, "the start replays the whole log");
    assert_eq!(recovered.stats.dump(), written.stats.dump(), "the dashboard record");
}

/// A node started again from its daily snapshot publishes, at the next day's first checkpoint, a snapshot whose
/// replay state, folded on from the one the start took, is the one a whole replay of its log carries there.
#[tokio::test]
async fn a_snapshot_written_after_a_start_from_a_snapshot_carries_what_the_whole_log_folds() {
    let written = Written::run("snapshot-start-next-day").await;
    let cfg = &written.cfg;
    let marks = marks::startup(cfg, written.key.marks_key(), false).unwrap();
    let recovered = recover(cfg, &written.key, None, &marks, None, &Metrics::new()).await.unwrap();
    assert_eq!(recovered.log.snapshot_sequence(), Some(written.snapshot.seq), "the start went on from the snapshot");
    let clock = Arc::new(TestClock::at(recovered.log.replayed.engine.now() + 1_000));
    let daily = cfg.daily_snapshots.as_ref().unwrap().dir.clone();
    let (manifest, _) = watch::channel(Manifest::default());
    let seq_cfg = SequencerConfig {
        clock: clock.clone(),
        genesis: genesis_commands(&cfg.genesis),
        daily_snapshots: Some(daily.clone()),
        ..sequencer_config(&cfg.node.log_dir, manifest)
    };
    let (stream, withdrawals) = (broadcast::channel(16).0, mpsc::unbounded_channel().0);
    let mut seq = Sequencer::recover_from(recovered.log, seq_cfg, written.key.clone(), stream, withdrawals).unwrap();
    seq.track_stats(recovered.stats.clone());
    seq.pipeline_log_syncs().unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let (tx, rx) = mpsc::channel(64);
    let sequencer = tokio::spawn(seq.run(rx));
    // Past the next midnight, the first checkpoint takes the day's snapshot.
    let to_midnight = Duration::from_millis(u64::try_from(DAY_MS - clock.now_ms().rem_euclid(DAY_MS)).unwrap());
    let two = Duration::from_secs(2);
    for by in [Duration::from_secs(61), to_midnight, two, two, two] {
        clock.advance(by);
        tx.send(Request::TimeSample(sample_around(clock.as_ref()))).await.unwrap();
        for market in MARKETS {
            submit(&tx, prices(market, clock.now_ms(), 67_500)).await;
        }
    }
    let listed = wait_for("the next day's snapshot", async {
        loop {
            let listed = read_manifest(&daily).unwrap();
            if listed.len() == 3 {
                return listed;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    drop(tx);
    sequencer.await.unwrap().unwrap();

    let newest = &listed[2];
    let offset = crate::log::Offset { seq: newest.seq, index: newest.index };
    let domain =
        crate::log::CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() };
    let signers = [AccountId(written.key.address())].into();
    let whole: serde_json::Value = serde_json::from_str(
        &crate::sequencer::replay::carried_at(&cfg.node.log_dir, offset, (domain, signers)).unwrap(),
    )
    .unwrap();
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(daily.join(&newest.file)).unwrap()).unwrap();
    assert_eq!(snapshot["carried"], whole, "the replay state the whole log folds at the snapshot's checkpoint");
}

// ── Helpers ──

/// Signs the daily snapshot at `path` again, with `alter` applied to the replay state and the history it carries,
/// under the test enclave's key in `cfg`'s deployment, its other parts as written.
fn resign(path: &std::path::Path, cfg: &NodeConfig, alter: impl FnOnce(&mut serde_json::Value, &mut History)) {
    use crate::daily_snapshots::{Covered, Signing, sha256};
    use serde_json::value::RawValue;
    #[derive(serde::Deserialize)]
    struct Parts {
        offset: crate::log::Offset,
        checkpoint: Checkpoint,
        engine: Box<RawValue>,
        carried: serde_json::Value,
        history: History,
    }
    let mut parts: Parts = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    alter(&mut parts.carried, &mut parts.history);
    let carried = serde_json::to_string(&parts.carried).unwrap();
    let history = serde_json::to_string(&parts.history).unwrap();
    let (engine, history) = (parts.engine.get(), history.as_str());
    let covered = Covered {
        state: sha256(engine.as_bytes()),
        carried: sha256(carried.as_bytes()),
        history: sha256(history.as_bytes()),
    };
    let domain =
        crate::log::CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() };
    let signing = Signing { key: Arc::new(EnclaveKey::test_key()), domain };
    let signature = signing.sign(parts.offset, parts.checkpoint.root.parse().unwrap(), covered);
    let (offset, checkpoint) =
        (serde_json::to_string(&parts.offset).unwrap(), serde_json::to_string(&parts.checkpoint).unwrap());
    let state = hex::encode(covered.state);
    let text = format!(
        r#"{{"schema":{},"offset":{offset},"checkpoint":{checkpoint},"engine":{engine},"state":"{state}","carried":{carried},"history":{history},"signature":"{signature}"}}"#,
        crate::daily_snapshots::SCHEMA
    );
    std::fs::write(path, text).unwrap();
}

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
