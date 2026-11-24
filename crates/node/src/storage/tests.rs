use super::layout::{AUTHENTICATED_PREFIX, Store};
use super::reading::Volume;
use super::*;

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::alerts::{Severity, capture};
use crate::config::StorageSection;
use crate::history::INDEX_FILE;
use crate::history::marks::store::MARKS_FILE;
use crate::metrics::Metrics;
use crate::snapshot::head::StoreId;
use crate::test_support::{capture_alerts_and_resolutions, scratch_dir};

mod scenario;

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
    holds(Store::MarksCache, 6 << 20);
    holds(Store::RecoveryCaches, 11 << 20);
    holds(Store::DailySnapshots, 13 << 20);
    holds(Store::AlertOutbox, 1 << 18);
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
    assert_eq!(volume.name, "log+history_index+marks_cache+alert_outbox");
    assert!(0 < volume.total_bytes && volume.free_bytes <= volume.total_bytes);
}

#[test]
fn a_directory_that_cannot_be_listed_is_a_failure_naming_the_store() {
    let root = scratch_dir("storage-unlistable");
    let dirs = Dirs::new(&root);
    // A file where the history directory should be.
    fs::remove_dir_all(&dirs.history).unwrap();
    write(&dirs.history, 10);
    let reading = dirs.layout().read();
    let stores: Vec<Store> = reading.failures.iter().map(|(store, _)| *store).collect();
    assert_eq!(stores, [Store::HistoryIndex, Store::MarksCache], "{:?}", reading.failures);
}

#[test]
fn the_volumes_are_named_by_the_stores_on_them() {
    let reading = Reading { volumes: vec![volume(&[Store::Log], 100 * GIB, 50 * GIB)], ..Reading::default() };
    metrics_text(&reading, |text| {
        assert!(text.contains("engine_volume_free_bytes{volume=\"log\"} 53687091200\n"), "{text}");
    });
}

#[test]
fn a_free_share_below_each_threshold_raises_its_alert_and_clears_with_a_margin() {
    let (mut watch, metrics) = (Watch::new(config()), Metrics::new());
    let t0 = Instant::now();
    let mut at = |free_percent: u64| {
        let reading = reading_with_free(free_percent);
        capture(|| watch.observe(t0, &reading, &metrics))
    };
    assert!(at(40).is_empty());
    let [low] = at(14).try_into().unwrap();
    assert_eq!((low.key(), low.severity), ("disk_space_low{volume=log}".to_owned(), Some(Severity::Warning)));
    assert!(low.summary.starts_with("volume log has 14.0 GiB free of 100.0 GiB (14.0%)"), "{}", low.summary);
    assert!(low.summary.ends_with("holds log 30.0 GiB"), "{}", low.summary);
    // Still low, and 15.5% is inside the margin a held level needs to clear: no new line.
    assert!(at(13).is_empty());
    assert!(at(16).is_empty());
    let lines = at(4);
    let [critical] = lines.as_slice() else { panic!("{lines:?}") };
    assert_eq!(
        (critical.key(), critical.severity),
        ("disk_space_critical{volume=log}".to_owned(), Some(Severity::Critical))
    );
    // Critical clears while the Warning stays open, then the Warning clears.
    assert!(at(5).is_empty());
    let [resolved] = at(8).try_into().unwrap();
    assert_eq!((resolved.key(), resolved.severity), (critical.key(), None));
    let [resolved] = at(60).try_into().unwrap();
    assert_eq!((resolved.key(), resolved.severity), (low.key(), None));
    assert!(at(60).is_empty());
}

#[test]
fn a_volume_that_jumps_from_clear_to_critical_raises_both_levels() {
    let (mut watch, metrics) = (Watch::new(config()), Metrics::new());
    let reading = reading_with_free(2);
    let keys: Vec<String> =
        capture(|| watch.observe(Instant::now(), &reading, &metrics)).iter().map(|line| line.key()).collect();
    assert_eq!(keys, ["disk_space_low{volume=log}", "disk_space_critical{volume=log}"]);
}

#[test]
fn a_volume_filling_within_a_threshold_alerts_on_its_growth_before_its_free_share_is_low() {
    let (mut watch, metrics) = (Watch::new(config()), Metrics::new());
    let t0 = Instant::now();
    let hour = Duration::from_secs(3600);
    let mut at = |hours: u32, used_gib: u64| {
        let reading = Reading {
            stores: vec![(Store::Log, used_gib * GIB)],
            volumes: vec![volume(&[Store::Log], 1000 * GIB, (1000 - used_gib) * GIB)],
            ..Reading::default()
        };
        capture(|| watch.observe(t0 + hour * hours, &reading, &metrics))
    };
    // A sixth of the six-hour window is sampled at hour one: 100 GiB an hour leaves 800 GiB free, eight hours.
    assert!(at(0, 100).is_empty());
    let [critical] = at(1, 200)
        .into_iter()
        .filter(|line| line.name == "disk_space_critical")
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    assert!(critical.summary.contains("growing 2400.0 GiB a day, full in 0.3 days"), "{}", critical.summary);
    metrics_of(&metrics, |text| {
        let growth: f64 = value(text, "engine_volume_growth_bytes_per_second{volume=\"log\"}");
        assert!((growth - 100.0 * GIB as f64 / 3600.0).abs() < 1.0, "{text}");
        let fill: f64 = value(text, "engine_volume_fill_seconds{volume=\"log\"}");
        assert!((fill - 28_800.0).abs() < 1.0, "{text}");
    });
    // Growth stops: the window slides past the growth, the projection goes, and both alerts resolve.
    let resolved = at(8, 200);
    assert!(resolved.iter().all(|line| line.severity.is_none()), "{resolved:?}");
    assert_eq!(resolved.len(), 2, "{resolved:?}");
    metrics_of(&metrics, |text| {
        assert!(!text.contains("engine_volume_fill_seconds{"), "{text}");
        assert!(!text.contains("engine_volume_growth"), "{text}");
    });
}

#[test]
fn no_projection_is_made_before_a_sixth_of_the_window_is_sampled() {
    let (mut watch, metrics) = (Watch::new(config()), Metrics::new());
    let t0 = Instant::now();
    let at = |minutes: u64, used_gib: u64| {
        let reading =
            Reading { volumes: vec![volume(&[Store::Log], 1000 * GIB, (1000 - used_gib) * GIB)], ..Reading::default() };
        (t0 + Duration::from_secs(60 * minutes), reading)
    };
    for (minutes, used) in [(0, 100), (59, 700)] {
        let (now, reading) = at(minutes, used);
        assert!(capture(|| watch.observe(now, &reading, &metrics)).is_empty());
    }
    metrics_of(&metrics, |text| assert!(!text.contains("engine_volume_growth"), "{text}"));
}

#[test]
fn an_unreadable_store_pages_once_and_resolves_when_it_reads_again() {
    let (mut watch, metrics) = (Watch::new(config()), Metrics::new());
    let t0 = Instant::now();
    let failing =
        Reading { failures: vec![(Store::DailySnapshots, "reading /d: denied".to_owned())], ..Reading::default() };
    let mut observe = |reading: &Reading| capture(|| watch.observe(t0, reading, &metrics));
    let [alert] = observe(&failing).try_into().unwrap();
    assert_eq!(
        (alert.key(), alert.severity),
        ("storage_unreadable{store=daily_snapshots}".to_owned(), Some(Severity::Warning))
    );
    assert_eq!(alert.summary, "cannot read daily_snapshots's size: reading /d: denied");
    assert!(observe(&failing).is_empty());
    let [resolved] = observe(&Reading::default()).try_into().unwrap();
    assert_eq!((resolved.key(), resolved.severity), (alert.key(), None));
}

#[test]
fn a_read_that_does_not_finish_pages_every_store_unreadable_until_one_does() {
    let mut watch = Watch::new(config());
    let metrics = Metrics::new();
    let [alert] = capture(|| watch.unread("the read has not finished in 30s")).try_into().unwrap();
    assert_eq!(alert.key(), "storage_unreadable{store=all}");
    let [resolved] = capture(|| watch.observe(Instant::now(), &Reading::default(), &metrics)).try_into().unwrap();
    assert_eq!((resolved.key(), resolved.severity), (alert.key(), None));
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

/// Defaults with a six-hour window, a sixth of it sampled at one hour.
fn config() -> StorageSection {
    StorageSection::default()
}

fn volume(stores: &[Store], total: u64, free: u64) -> Volume {
    let name = stores.iter().map(|store| store.name()).collect::<Vec<_>>().join("+");
    Volume { name, stores: stores.to_vec(), total_bytes: total, free_bytes: free }
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

fn metrics_of(metrics: &Metrics, check: impl FnOnce(&str)) {
    check(&metrics.encode());
}
