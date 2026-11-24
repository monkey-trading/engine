use super::layout::{AUTHENTICATED_PREFIX, Store};
use super::reading::{Device, Volume, name_volumes};
use super::*;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::alerts::{Severity, capture};
use crate::config::StorageSection;
use crate::history::INDEX_FILE;
use crate::history::marks::store::MARKS_FILE;
use crate::metrics::Metrics;
use crate::node::marks::{PRIOR_FILE, SEED_FILE};
use crate::snapshot::head::StoreId;
use crate::test_support::{capture_alerts_and_resolutions, scratch_dir};

mod run;
mod scenario;
mod watching;

const GIB: u64 = 1 << 30;

#[test]
fn each_store_counts_its_own_files() {
    let root = scratch_dir("storage-attribution");
    let dirs = Dirs::new(&root);
    let id = StoreId([7; 32]).path(&dirs.snapshots);
    write(&dirs.log.join("segment-0.jsonl"), 3 << 20);
    write(&dirs.log.join("manifest.jsonl"), 1 << 20);
    write(&dirs.history.join(INDEX_FILE), 2 << 20);
    write(&dirs.history.join(format!("{INDEX_FILE}-wal")), 1 << 20);
    write(&dirs.history.join(MARKS_FILE), 5 << 20);
    write(&dirs.history.join(format!("{MARKS_FILE}-wal")), 1 << 20);
    write(&dirs.history.join(PRIOR_FILE), 3 << 20);
    write(&dirs.history.join(SEED_FILE), 1 << 20);
    write(&id, 7 << 20);
    write(&dirs.snapshots.join("cache-1.bin"), 11 << 20);
    write(&dirs.daily.join("2026-11-23.snapshot"), 13 << 20);
    write(&dirs.outbox.join("outbox.sqlite"), 1 << 18);

    let reading = dirs.layout().read();
    assert!(reading.failures.is_empty(), "{:?}", reading.failures);
    let size = |store| reading.stores.iter().find(|(s, _)| *s == store).unwrap().1;
    // The disk allocates whole blocks, so a store holds at least what was written and a little more.
    let holds = |store, written: u64| {
        let found = size(store);
        assert!(found >= written && found < written + (1 << 20), "{store:?}: {found} for {written} written");
    };
    holds(Store::Log, 4 << 20);
    holds(Store::HistoryIndex, (3 << 20) + (7 << 20));
    holds(Store::MarksCache, 10 << 20);
    holds(Store::RecoveryCaches, 11 << 20);
    holds(Store::DailySnapshots, 13 << 20);
    holds(Store::AlertOutbox, 1 << 18);
    // Named by its first store, so adding or removing an optional one on it leaves its name as it was.
    let [volume] = reading.volumes.as_slice() else { panic!("{:?}", reading.volumes) };
    assert_eq!(volume.name, "log");
    assert_eq!(volume.stores.len(), 6);
}

#[test]
fn a_sealed_segment_is_statted_once_and_the_two_newest_each_read() {
    let root = scratch_dir("storage-segments");
    let dirs = Dirs::new(&root);
    let layout = dirs.layout();
    let segment = |seq: u64| dirs.log.join(crate::log::segment_name(seq));
    let log_bytes = |layout: &Layout| layout.read().stores.iter().find(|(store, _)| *store == Store::Log).unwrap().1;
    for seq in [0, 1000, 2000, 3000] {
        write(&segment(seq), 1 << 20);
    }
    write(&dirs.log.join("manifest.jsonl"), 1 << 16);
    let first = log_bytes(&layout);
    assert!(first >= (4 << 20) + (1 << 16), "{first}");
    // The sealed first segment grows behind the cache's back: not counted again while it is listed.
    write(&segment(0), 3 << 20);
    assert_eq!(log_bytes(&layout), first);
    // The two newest, and every other file, are read each time.
    write(&segment(3000), 2 << 20);
    write(&dirs.log.join("manifest.jsonl"), 1 << 17);
    let grown = log_bytes(&layout);
    assert!(grown >= first + (1 << 20) + (1 << 16), "{grown} against {first}");
    // A segment gone from the listing leaves the total, and a new one joins it.
    fs::remove_file(segment(1000)).unwrap();
    let removed = log_bytes(&layout);
    assert!(removed + (1 << 20) <= grown + (1 << 16), "{removed} against {grown}");
    write(&segment(4000), 1 << 20);
    assert!(log_bytes(&layout) >= removed + (1 << 20));
}

#[test]
fn the_file_names_the_stores_pick_by_are_the_ones_the_node_writes() {
    let id = StoreId([0xab; 32]).path(Path::new("dir"));
    assert!(id.file_name().unwrap().to_str().unwrap().starts_with(AUTHENTICATED_PREFIX), "{id:?}");
    assert!(!INDEX_FILE.starts_with(MARKS_FILE) && !MARKS_FILE.starts_with(INDEX_FILE));
    assert!(!INDEX_FILE.starts_with(AUTHENTICATED_PREFIX));
}

#[test]
fn a_store_that_does_not_exist_yet_holds_nothing_and_is_no_failure() {
    let root = scratch_dir("storage-missing");
    let layout = Layout::new(root.join("log"), root.join("history"), None, None, root.join("alerts"));
    let reading = layout.read();
    assert!(reading.failures.is_empty(), "{:?}", reading.failures);
    assert!(reading.stores.iter().all(|(_, bytes)| *bytes == 0), "{:?}", reading.stores);
    // The volume of the nearest existing ancestor, the scratch directory's parent.
    let [volume] = reading.volumes.as_slice() else { panic!("{:?}", reading.volumes) };
    assert_eq!(volume.name, "log");
    assert_eq!(volume.stores, [Store::Log, Store::HistoryIndex, Store::MarksCache, Store::AlertOutbox]);
    assert!(0 < volume.total_bytes && volume.free_bytes <= volume.total_bytes);
}

#[test]
fn a_directory_that_cannot_be_listed_is_a_failure_and_its_store_keeps_its_last_size() {
    let root = scratch_dir("storage-unlistable");
    let dirs = Dirs::new(&root);
    write(&dirs.log.join("segment-0.jsonl"), 1 << 20);
    write(&dirs.history.join(INDEX_FILE), 2 << 20);
    let metrics = Metrics::new();
    let reading = dirs.layout().read();
    reading.record(&metrics);
    let before = metrics_of(&metrics, |text| value(text, "engine_store_bytes{store=\"history_index\"}"));
    assert!(before >= f64::from(2 << 20), "{before}");

    // A file where the history directory should be.
    fs::remove_dir_all(&dirs.history).unwrap();
    write(&dirs.history, 10);
    let reading = dirs.layout().read();
    let failed: Vec<Store> = reading.failures.iter().map(|(store, _)| *store).collect();
    assert_eq!(failed, [Store::HistoryIndex, Store::MarksCache], "{:?}", reading.failures);
    let sized: Vec<Store> = reading.stores.iter().map(|(store, _)| *store).collect();
    assert_eq!(sized, [Store::Log, Store::RecoveryCaches, Store::DailySnapshots, Store::AlertOutbox], "{sized:?}");
    reading.record(&metrics);
    let after = metrics_of(&metrics, |text| value(text, "engine_store_bytes{store=\"history_index\"}"));
    assert_eq!(after, before, "an unread store's gauge is left as it was");
}

#[test]
fn volumes_are_named_by_their_first_store_whatever_else_is_on_them() {
    let device =
        |stores: &[Store]| Device { stores: stores.to_vec(), holds: vec![], space: Some((100 * GIB, 50 * GIB)) };
    let names = |devices: Vec<(u64, Device)>| {
        name_volumes(devices.into_iter().collect()).into_iter().map(|volume| volume.name).collect::<Vec<_>>()
    };
    // The log's own volume, and a data volume whose optional stores come and go.
    let data = [Store::AlertOutbox, Store::HistoryIndex, Store::MarksCache];
    assert_eq!(names(vec![(1, device(&[Store::Log])), (2, device(&data))]), ["log", "history_index"]);
    let with_more = [Store::DailySnapshots, Store::RecoveryCaches, Store::AlertOutbox, Store::HistoryIndex];
    assert_eq!(names(vec![(1, device(&[Store::Log])), (2, device(&with_more))]), ["log", "history_index"]);
    // The snapshots directory on a volume of its own is named for the store that owns it, not the history index.
    let split = vec![(1, device(&[Store::Log])), (2, device(&data)), (3, device(&[Store::RecoveryCaches]))];
    assert_eq!(names(split), ["log", "history_index", "recovery_caches"]);
}

#[test]
fn a_volumes_space_is_exported_under_its_name() {
    let reading = Reading { volumes: vec![volume(&[Store::Log], 100 * GIB, 50 * GIB)], ..Reading::default() };
    metrics_text(&reading, |text| {
        assert!(text.contains("engine_volume_free_bytes{volume=\"log\"} 53687091200\n"), "{text}");
    });
}

#[test]
fn the_default_thresholds_validate_and_each_inconsistent_one_is_refused() {
    let mut section = StorageSection::default();
    section.validate().unwrap();
    for (name, change) in [
        (
            "critical not below warning",
            (|s: &mut StorageSection| s.critical_free_percent = 15) as fn(&mut StorageSection),
        ),
        ("zero critical share", |s| s.critical_free_percent = 0),
        ("share over 100", |s| s.warn_free_percent = 101),
        ("critical fill not below warning", |s| s.critical_fill_within = s.warn_fill_within),
        ("interval under a second", |s| s.interval = Duration::from_millis(999)),
        ("window under six intervals", |s| s.rate_window = s.interval * 5),
        ("window over a week", |s| s.rate_window = Duration::from_secs(8 * 24 * 3600)),
    ] {
        let mut bad = section.clone();
        change(&mut bad);
        assert!(bad.validate().is_err(), "{name}");
    }
    section.warn_free_percent = 100;
    section.validate().unwrap();
}

// ── Helpers ──

struct Dirs {
    log: std::path::PathBuf,
    history: std::path::PathBuf,
    snapshots: std::path::PathBuf,
    daily: std::path::PathBuf,
    outbox: std::path::PathBuf,
}

impl Dirs {
    /// The five store directories under `root`, created.
    fn new(root: &Path) -> Self {
        let dir = |name: &str| {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            dir
        };
        Self {
            log: dir("log"),
            history: dir("history"),
            snapshots: dir("snapshots"),
            daily: dir("daily"),
            outbox: dir("alerts"),
        }
    }

    fn layout(&self) -> Layout {
        Layout::new(
            self.log.clone(),
            self.history.clone(),
            Some(self.snapshots.clone()),
            Some(self.daily.clone()),
            self.outbox.clone(),
        )
    }
}

/// A file of `len` bytes that are not zero, so no filesystem stores it sparse.
fn write(path: &Path, len: usize) {
    fs::write(path, vec![0xa5; len]).unwrap();
}

const HOUR: Duration = Duration::from_secs(3600);

/// The defaults: a six-hour window sampled every minute.
fn config() -> StorageSection {
    StorageSection::default()
}

/// A watch that has judged every store readable, so what a test raises or resolves is its own.
fn primed_watch() -> Watch {
    let mut watch = Watch::new(config());
    let reading = Reading {
        stores: vec![
            (Store::Log, 0),
            (Store::HistoryIndex, 0),
            (Store::MarksCache, 0),
            (Store::RecoveryCaches, 0),
            (Store::DailySnapshots, 0),
            (Store::AlertOutbox, 0),
        ],
        ..Reading::default()
    };
    capture(|| watch.observe(Instant::now(), &reading, &Metrics::new()));
    watch
}

/// A 1000 GiB volume holding the log, with `used_gib` of it used.
fn filling(used_gib: u64) -> Reading {
    Reading {
        stores: vec![(Store::Log, used_gib * GIB)],
        volumes: vec![volume(&[Store::Log], 1000 * GIB, (1000 - used_gib) * GIB)],
        ..Reading::default()
    }
}

/// A volume of the given size holding 30 GiB of each of `stores`, named by the first.
fn volume(stores: &[Store], total: u64, free: u64) -> Volume {
    let holds = stores.iter().map(|store| (*store, 30 * GIB)).collect();
    Volume { name: stores[0].name().to_owned(), stores: stores.to_vec(), holds, total_bytes: total, free_bytes: free }
}

/// A 100 GiB volume holding the log, with `free_percent` of it free and the log at 30 GiB.
fn reading_with_free(free_percent: u64) -> Reading {
    Reading {
        stores: vec![(Store::Log, 30 * GIB)],
        volumes: vec![volume(&[Store::Log], 100 * GIB, 100 * GIB / 100 * free_percent)],
        ..Reading::default()
    }
}

/// The value of the series that `text` exports with the name and labels `series`.
fn value(text: &str, series: &str) -> f64 {
    let line = text.lines().find(|line| line.starts_with(&format!("{series} "))).expect("series exported");
    line.rsplit(' ').next().unwrap().parse().unwrap()
}

fn metrics_text(reading: &Reading, check: impl FnOnce(&str)) {
    let metrics = Metrics::new();
    reading.record(&metrics);
    metrics_of(&metrics, check);
}

fn metrics_of<T>(metrics: &Metrics, check: impl FnOnce(&str) -> T) -> T {
    check(&metrics.encode())
}
