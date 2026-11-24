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
use crate::node::marks::{PRIOR_FILE, SEED_FILE};
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
    let segment = |seq: u64| dirs.log.join(format!("{seq:020}.jsonl"));
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
fn a_volumes_space_is_exported_under_its_name() {
    let reading = Reading { volumes: vec![volume(&[Store::Log], 100 * GIB, 50 * GIB)], ..Reading::default() };
    metrics_text(&reading, |text| {
        assert!(text.contains("engine_volume_free_bytes{volume=\"log\"} 53687091200\n"), "{text}");
    });
}

#[test]
fn a_free_share_below_each_threshold_raises_its_alert_and_clears_with_a_margin() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    let mut at = |free_percent: u64| {
        let reading = reading_with_free(free_percent);
        capture(|| watch.observe(t0, &reading, &metrics))
    };
    assert!(only_alerts(at(40)).is_empty());
    let [low] = at(14).try_into().unwrap();
    assert_eq!((low.key(), low.severity), ("disk_space_low{volume=log}".to_owned(), Some(Severity::Warning)));
    assert!(low.summary.starts_with("volume log has 14.0 GiB free of 100.0 GiB (14.0%)"), "{}", low.summary);
    assert!(low.summary.ends_with("holds log 30.0 GiB"), "{}", low.summary);
    // Still low, and 16% is inside the 16.5% a held Warning needs to clear: no new line.
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
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let reading = reading_with_free(2);
    let keys: Vec<String> =
        capture(|| watch.observe(Instant::now(), &reading, &metrics)).iter().map(|line| line.key()).collect();
    assert_eq!(keys, ["disk_space_low{volume=log}", "disk_space_critical{volume=log}"]);
}

#[test]
fn a_volumes_first_judgement_resolves_the_levels_it_is_not_at_for_an_episode_a_run_before_left_open() {
    let metrics = Metrics::new();
    let t0 = Instant::now();
    let first = |free_percent| {
        let (mut watch, reading) = (Watch::new(config()), reading_with_free(free_percent));
        let lines = capture(|| watch.observe(t0, &reading, &metrics));
        lines
            .iter()
            .filter(|line| line.name.starts_with("disk_space"))
            .map(|line| (line.key(), line.severity))
            .collect::<Vec<_>>()
    };
    let resolved = |name: &str| (format!("{name}{{volume=log}}"), None);
    let raised = |name: &str, severity| (format!("{name}{{volume=log}}"), Some(severity));
    // Repaired during the stop: both episodes the earlier run left open end.
    let clear = first(60);
    assert!(
        clear.contains(&resolved("disk_space_low")) && clear.contains(&resolved("disk_space_critical")),
        "{clear:?}"
    );
    // Still low: the Warning is raised again, which pages nothing within its quiet period, and the Critical ends.
    assert_eq!(first(10), [raised("disk_space_low", Severity::Warning), resolved("disk_space_critical")]);
    assert_eq!(
        first(2),
        [raised("disk_space_low", Severity::Warning), raised("disk_space_critical", Severity::Critical)]
    );
    // A watch that has judged the volume says nothing of a clear one again.
    let mut watch = Watch::new(config());
    let reading = reading_with_free(60);
    capture(|| watch.observe(t0, &reading, &metrics));
    assert!(capture(|| watch.observe(t0, &reading, &metrics)).is_empty());
}

#[test]
fn a_volume_filling_within_the_warning_time_alerts_alone_and_clears_with_a_margin() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    let mut at = |hours: u32, used_gib: u64| capture(|| watch.observe(t0 + HOUR * hours, &filling(used_gib), &metrics));
    // The window is six hours: nothing is projected until it is sampled, whatever the first hours wrote.
    assert!(only_alerts(at(0, 100)).is_empty());
    // 3 GiB an hour leaves 882 GiB free, about 12 days: inside the 14-day warning, past the 3-day critical.
    let [low] = at(6, 118).try_into().unwrap();
    assert_eq!((low.key(), low.severity), ("disk_space_low{volume=log}".to_owned(), Some(Severity::Warning)));
    // 2.5 GiB an hour, 867 GiB free: 14.5 days is past the warning time and inside the 15.4 days a held one clears at.
    assert!(at(12, 133).is_empty());
    // 2.17 GiB an hour, 854 GiB free: 16.4 days.
    let [resolved] = at(18, 146).try_into().unwrap();
    assert_eq!((resolved.key(), resolved.severity), (low.key(), None));
}

#[test]
fn a_volume_filling_within_the_critical_time_raises_critical_and_exports_its_projection() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    let mut at = |hours: u32, used_gib: u64| capture(|| watch.observe(t0 + HOUR * hours, &filling(used_gib), &metrics));
    assert!(only_alerts(at(0, 100)).is_empty());
    // 100 GiB an hour leaves 300 GiB free, three hours.
    let lines = at(6, 700);
    let [low, critical] = lines.as_slice() else { panic!("{lines:?}") };
    assert_eq!((low.name.as_str(), critical.name.as_str()), ("disk_space_low", "disk_space_critical"));
    assert!(critical.summary.contains("growing 2400.0 GiB a day, full in 0.1 days"), "{}", critical.summary);
    metrics_of(&metrics, |text| {
        let growth = value(text, "engine_volume_growth_bytes_per_second{volume=\"log\"}");
        assert!((growth - 100.0 * GIB as f64 / 3600.0).abs() < 1.0, "{text}");
        let fill = value(text, "engine_volume_fill_seconds{volume=\"log\"}");
        assert!((fill - 10_800.0).abs() < 1.0, "{text}");
    });
    // Growth stops: the window slides past it, the projection goes, and both alerts resolve.
    let resolved = at(12, 700);
    assert!(resolved.iter().all(|line| line.severity.is_none()) && resolved.len() == 2, "{resolved:?}");
    metrics_of(&metrics, |text| {
        assert!(!text.contains("engine_volume_fill_seconds{"), "{text}");
        assert_eq!(value(text, "engine_volume_growth_bytes_per_second{volume=\"log\"}"), 0.0);
    });
}

#[test]
fn a_steady_growth_keeps_its_projection_through_a_window_of_uneven_reads() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    let (mut seconds, mut raised, mut resolved) = (0u64, 0, 0);
    // Reads land 55 to 65 seconds apart, as a read's own time moves each one, for seven hours of 3 GiB an hour.
    for step in 0..450u64 {
        let used = 100 * GIB + 3 * GIB * seconds / 3600;
        let reading =
            Reading { volumes: vec![volume(&[Store::Log], 1000 * GIB, 1000 * GIB - used)], ..Reading::default() };
        let lines = capture(|| watch.observe(t0 + Duration::from_secs(seconds), &reading, &metrics));
        let lines = if step == 0 { only_alerts(lines) } else { lines };
        raised += lines.iter().filter(|line| line.severity.is_some()).count();
        resolved += lines.iter().filter(|line| line.severity.is_none()).count();
        if seconds >= 6 * 3600 {
            let projected = metrics_of(&metrics, |text| text.contains("engine_volume_fill_seconds{"));
            assert!(projected, "no projection at {seconds} s");
        }
        seconds += 55 + step * 7 % 11;
    }
    assert!(seconds > 6 * 3600 + 3600, "the loop ran only {seconds} s");
    // The 12-day projection raises the Warning once, when the window fills, and never flaps.
    assert_eq!((raised, resolved), (1, 0));
}

#[test]
fn a_burst_in_the_windows_first_hour_projects_nothing_until_the_window_is_full() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    // A 1.92 TB volume whose index catches up 23 GiB in the first hour of a start, then stops: read at the
    // hour's endpoints that is 23 GiB an hour, a fill in under four days.
    let mut at = |minutes: u64, used_gib: u64| {
        let reading =
            Reading { volumes: vec![volume(&[Store::Log], 1920 * GIB, (1920 - used_gib) * GIB)], ..Reading::default() };
        capture(|| watch.observe(t0 + Duration::from_secs(60 * minutes), &reading, &metrics))
    };
    assert!(only_alerts(at(0, 100)).is_empty());
    assert!(at(60, 123).is_empty());
    metrics_of(&metrics, |text| assert!(!text.contains("engine_volume_growth"), "{text}"));
    // Over the whole window it is 3.8 GiB an hour, about 20 days to fill.
    assert!(at(360, 123).is_empty());
    metrics_of(&metrics, |text| {
        let growth = value(text, "engine_volume_growth_bytes_per_second{volume=\"log\"}");
        assert!((growth - 23.0 * GIB as f64 / 21_600.0).abs() < 1.0, "{text}");
    });
}

#[test]
fn an_unreadable_store_pages_once_and_resolves_when_it_reads_again() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    let failing =
        Reading { failures: vec![(Store::DailySnapshots, "reading /d: denied".to_owned())], ..Reading::default() };
    let healthy = Reading { stores: vec![(Store::DailySnapshots, 0)], ..Reading::default() };
    let mut observe = |reading: &Reading| capture(|| watch.observe(t0, reading, &metrics));
    let [alert] = observe(&failing).try_into().unwrap();
    assert_eq!(
        (alert.key(), alert.severity),
        ("storage_unreadable{store=daily_snapshots}".to_owned(), Some(Severity::Warning))
    );
    assert_eq!(alert.summary, "cannot read daily_snapshots's size: reading /d: denied");
    assert!(observe(&failing).is_empty());
    let [resolved] = observe(&healthy).try_into().unwrap();
    assert_eq!((resolved.key(), resolved.severity), (alert.key(), None));
}

#[test]
fn a_first_healthy_read_resolves_each_stores_unreadable_episode_a_run_before_left_open() {
    let mut watch = Watch::new(config());
    let reading = Reading { stores: vec![(Store::Log, 0), (Store::AlertOutbox, 0)], ..Reading::default() };
    let lines = capture(|| watch.observe(Instant::now(), &reading, &Metrics::new()));
    let keys: Vec<String> = lines.iter().map(|line| line.key()).collect();
    assert_eq!(
        keys,
        ["storage_unreadable{store=log}", "storage_unreadable{store=alert_outbox}", "storage_unreadable{store=all}"]
    );
    assert!(lines.iter().all(|line| line.severity.is_none()));
}

#[test]
fn a_read_that_does_not_finish_pages_all_and_leaves_each_stores_own_episode() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let failing =
        Reading { failures: vec![(Store::DailySnapshots, "reading /d: denied".to_owned())], ..Reading::default() };
    capture(|| watch.observe(Instant::now(), &failing, &metrics));
    let [alert] = capture(|| watch.unread("the read has not finished in 30s")).try_into().unwrap();
    assert_eq!(alert.key(), "storage_unreadable{store=all}");
    // The next read that finishes ends `all`, and the store's own episode with it only when it reads.
    let healthy = Reading { stores: vec![(Store::DailySnapshots, 0)], ..Reading::default() };
    let keys: Vec<String> =
        capture(|| watch.observe(Instant::now(), &healthy, &metrics)).iter().map(|line| line.key()).collect();
    assert_eq!(keys, ["storage_unreadable{store=daily_snapshots}", "storage_unreadable{store=all}"]);
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

/// A watch that has judged every store readable, so what a test raises or resolves is its own. A volume's first
/// judgement still resolves the levels it is not at, which `only_alerts` leaves out.
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

/// The lines that raise an alert, without the resolutions a first judgement adds.
fn only_alerts(lines: Vec<crate::alerts::AlertLine>) -> Vec<crate::alerts::AlertLine> {
    lines.into_iter().filter(|line| line.severity.is_some()).collect()
}

/// A 1000 GiB volume holding the log, with `used_gib` of it used.
fn filling(used_gib: u64) -> Reading {
    Reading {
        stores: vec![(Store::Log, used_gib * GIB)],
        volumes: vec![volume(&[Store::Log], 1000 * GIB, (1000 - used_gib) * GIB)],
        ..Reading::default()
    }
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

fn metrics_of<T>(metrics: &Metrics, check: impl FnOnce(&str) -> T) -> T {
    check(&metrics.encode())
}
