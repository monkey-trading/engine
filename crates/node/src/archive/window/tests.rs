use super::*;

use std::collections::BTreeMap;
use std::path::Path;

use engine_core::command::Command;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::archive::read::origin::{bucket_of, upload};
use crate::archive::stand_in::StandIn;
use crate::archive::whole_ranges;
use crate::clock::test_clock::TestClock;
use crate::daily_snapshots::{Listed, MANIFEST as SNAPSHOTS};
use crate::log::{LogEntry, LogReader, Offset};
use crate::sequencer::{Request, genesis_commands};
use crate::test_support::{
    Alert, TestSequencer, capture_alerts_and_resolutions, dev_config, log_entries, sample_around, scratch_dir,
};

mod fresh;
mod restore;

/// The sequenced time the fixtures' logs start at.
const START: Millis = 1_792_489_800_000;
const MINUTE_MS: Millis = 60_000;

/// A dev node's log of ticks a minute apart on a test clock, with the bucket's stand-in and the node's directories of
/// daily snapshots and history.
pub(super) struct Fixture {
    pub dir: PathBuf,
    pub snapshots: PathBuf,
    pub history: PathBuf,
    pub clock: Arc<TestClock>,
    pub node: TestSequencer,
    pub bucket: StandIn,
}

impl Fixture {
    /// A log in a new directory named for `name`, of `minutes` ticks a minute apart after its genesis.
    pub(super) async fn start(name: &str, minutes: u64) -> Self {
        let run = scratch_dir(name);
        let (dir, snapshots, history) = (run.join("log"), run.join("snapshots"), run.join("history"));
        std::fs::create_dir_all(&snapshots).unwrap();
        let clock = Arc::new(TestClock::at(START));
        let node = TestSequencer::start_at(&dir, 1024, clock.clone(), genesis_commands(&dev_config().genesis));
        let fixture = Self { dir, snapshots, history, clock, node, bucket: StandIn::start().await };
        fixture.ticks(minutes).await;
        fixture
    }

    /// Sequences `minutes` ticks, each a minute after the one before.
    pub(super) async fn ticks(&self, minutes: u64) {
        for _ in 0..minutes {
            self.clock.advance(Duration::from_secs(60));
            self.node.tx.send(Request::TimeSample(sample_around(self.clock.as_ref()))).await.unwrap();
            self.node.submit(Command::Tick).await;
        }
    }

    /// The writer's view of the log.
    pub(super) fn view(&self) -> Manifest {
        self.node.manifest.borrow().clone()
    }

    /// Puts the log's whole ranges the bucket lacks into it, as the uploader does, and returns how many it holds.
    pub(super) fn archive_all(&self) -> u64 {
        let view = self.view();
        let whole = whole_ranges(&view).unwrap();
        for index in 0..whole {
            let key = crate::archive::range_key(&crate::archive::stand_in::test_log(), index);
            if !self.bucket.with(|b| b.objects.contains_key(&key)) {
                let raw = crate::archive::range_bytes(&self.dir, &view, index).expect("a whole range on the host");
                self.bucket.with(|b| b.objects.insert(key, crate::archive::compress(&raw).unwrap()));
            }
        }
        whole
    }

    /// The seq and stamp of the first command stamped `minutes` or more after the log's start.
    pub(super) fn at_minute(&self, minutes: Millis) -> (u64, Millis) {
        commands(&self.dir).into_iter().find(|(_, ts)| *ts >= START + minutes * MINUTE_MS).expect("a command that late")
    }

    /// Lists a daily snapshot at the command `minutes` into the log, of the format that carries the replay state, as
    /// the node holds one; returns its seq.
    pub(super) fn snapshot_at(&self, minutes: Millis) -> u64 {
        let (seq, ts) = self.at_minute(minutes);
        let file = format!("{seq:020}-{:010}.json", 1);
        let body = b"{\"schema\":3,\"offset\":\"0:0\"}";
        std::fs::write(self.snapshots.join(&file), body).unwrap();
        let (root, bytes, sha256) = (format!("0x{seq:064x}"), body.len() as u64, hex::encode(Sha256::digest(body)));
        let listing = Listed { file, seq, index: 1, ts, root, bytes, sha256 };
        let mut manifest = std::fs::read(self.snapshots.join(SNAPSHOTS)).unwrap_or_default();
        manifest.extend(serde_json::to_vec(&listing).unwrap());
        manifest.push(b'\n');
        std::fs::write(self.snapshots.join(SNAPSHOTS), manifest).unwrap();
        seq
    }

    /// A window of `window` minutes over the log, the history keeping `retention` minutes, below `archived` ranges.
    pub(super) fn window(
        &self,
        (window, retention): (Millis, Millis),
        archived: u64,
    ) -> (Window, watch::Sender<Option<u64>>) {
        let history = HistorySection {
            dir: self.history.clone(),
            retention: Duration::from_millis((retention * MINUTE_MS) as u64),
            marks: None,
        };
        let restore =
            RestoreSources { http: self.bucket.client.clone(), bucket: bucket_of(&self.bucket), nodes: vec![] };
        let (sender, archived) = watch::channel(Some(archived));
        let window = Window::new(self.dir.clone(), window * MINUTE_MS, self.snapshots.clone(), history, restore)
            .following(watch::channel(None).1, archived, Arc::new(Metrics::new()));
        (window, sender)
    }

    /// The stamp of the first command of the newest completed segment: the window's measure of now.
    pub(super) fn now(&self) -> Millis {
        let newest = self.view().segments.last().cloned().expect("a completed segment");
        commands(&self.dir).into_iter().find(|(seq, _)| *seq == newest.first_seq).expect("its first command").1
    }
}

/// Each command of the log in `dir` that the host holds, its seq and stamp.
pub(super) fn commands(dir: &Path) -> Vec<(u64, Millis)> {
    let first = list_segments(dir).unwrap().first().map_or(0, |(seq, _)| *seq);
    let mut commands = Vec::new();
    crate::log::SegmentedLog::open_from(dir, Offset::start_of(first), |entry| {
        if let LogEntry::Command(command) = entry {
            commands.push((command.sequenced.seq, command.sequenced.ts));
        }
        Ok(())
    })
    .expect("the log on the host reads");
    commands
}

/// Whether the host holds `listed`'s file in `dir`.
pub(super) fn held(dir: &Path, listed: &SegmentInfo) -> bool {
    dir.join(&listed.file).exists()
}

/// A segment leaves only once every range it holds seqs of is in the bucket, its every command is older than the
/// window, it ends before both floors, and the bucket gives back its bytes: here the daily snapshot a start goes on
/// from binds, and every segment before it, past the window, leaves.
#[tokio::test]
async fn segments_before_the_snapshot_a_start_goes_on_from_leave_once_the_bucket_gives_them_back() {
    let fixture = Fixture::start("window-leave", 150).await;
    let ranges = fixture.archive_all();
    let snapshot = fixture.snapshot_at(60);
    let (mut window, _archived) = fixture.window((30, 30), ranges);
    let (view, now) = (fixture.view(), fixture.now());
    let stamps: BTreeMap<u64, Millis> = commands(&fixture.dir).into_iter().collect();
    let pass = window.pass(&view).await.unwrap();
    assert_eq!(pass.held, Some(Held::Snapshot), "{pass:?}");
    assert!(!pass.removed.is_empty(), "segments past the window left");
    for (at, listed) in view.segments.iter().enumerate() {
        let removed = pass.removed.contains(&listed.file);
        assert_eq!(removed, !held(&fixture.dir, listed), "{}", listed.file);
        assert_eq!(removed, listed.last_seq < snapshot, "{}: only those before the snapshot leave", listed.file);
        if removed {
            let next = &view.segments[at + 1];
            assert!(stamps[&next.first_seq] + 30 * MINUTE_MS <= now, "{}: past the window", listed.file);
            assert!(listed.last_seq < ranges * range_seqs(), "{}: in the bucket", listed.file);
        }
    }
    assert_eq!(window.metrics.archive.window.removed.get(), pass.removed.len() as u64);
    let first = view.segments.iter().find(|listed| held(&fixture.dir, listed)).unwrap();
    assert_eq!(window.metrics.archive.window.local_first_seq.get() as u64, first.first_seq);
    // A second pass finds nothing more to remove, and the log reads from its first segment on the host.
    assert_eq!(window.pass(&view).await.unwrap().removed, Vec::<String>::new());
    let mut reader = LogReader::open(&fixture.dir, Offset::start_of(first.first_seq), fixture.node.manifest.clone())
        .expect("a read from the first segment on the host");
    assert!(reader.next_entry().unwrap().is_some());
    fixture.node.stop().await;
}

/// A segment holding a seq of a range the bucket does not hold yet stays, whatever its age, whatever else holds its
/// commands, such as a checkpoint's tail in the bucket, and whatever else lets it go: as the one a log's last,
/// unfilled range starts in, which a log that stops for good leaves, its commands in the bucket only in tails.
#[tokio::test]
async fn a_segment_holding_a_seq_of_a_range_the_bucket_lacks_stays_however_old() {
    let fixture = Fixture::start("window-archive", 150).await;
    let ranges = fixture.archive_all();
    for minutes in [20, 60, 100] {
        fixture.snapshot_at(minutes);
    }
    for archived in [0, 3, 7, ranges / 2] {
        let (mut window, _archived) = fixture.window((30, 30), archived);
        window.pass(&fixture.view()).await.unwrap();
        for listed in fixture.view().segments.iter().filter(|listed| !held(&fixture.dir, listed)) {
            assert!(listed.last_seq < archived * range_seqs(), "{} left before range {archived}", listed.file);
        }
    }
    let view = fixture.view();
    let kept = view.segments.iter().find(|listed| held(&fixture.dir, listed)).cloned().unwrap();
    let (mut window, _archived) = fixture.window((30, 30), 0);
    let mut floors = Some(Floors { snapshot: Some(u64::MAX), history: Some(u64::MAX) });
    let ages = (kept.last_seq, Millis::MAX / 2);
    let holding = window.holding(&view, &kept, ages, &mut floors, 0).await.unwrap();
    assert_eq!(holding, Some(Held::Archive), "a segment holding the first seq the bucket lacks, however old");
    let ages = (kept.last_seq + 1, Millis::MAX / 2);
    assert_eq!(window.holding(&view, &kept, ages, &mut floors, 0).await.unwrap(), None, "the bucket holds it");
    // A checkpoint's tail in the bucket, through the unfilled range, counts for nothing: the segment holding the
    // range's seqs stays, and the bucket gives back no range for it.
    let whole = whole_ranges(&view).unwrap();
    let unfilled = view.segments.iter().find(|listed| listed.last_seq >= whole * range_seqs()).cloned();
    let unfilled = unfilled.expect("a completed segment holds a seq of the unfilled range");
    // The key's root stands for the checkpoint's: nothing the window reads names a tail.
    let (seq, root) = (fixture.node.tick_to_checkpoint().await, engine_core::eip712::B256::ZERO);
    fixture.ticks(1).await;
    let tail_view = fixture.view();
    for index in whole..=seq / range_seqs() {
        let piece = crate::archive::piece_bytes(&fixture.dir, &tail_view, index, seq).unwrap();
        let key = crate::archive::stand_in::tail_key(seq, &root, index);
        fixture.bucket.with(|b| b.objects.insert(key, crate::archive::compress(&piece).unwrap()));
    }
    let ages = (whole * range_seqs(), Millis::MAX / 2);
    let holding = window.holding(&view, &unfilled, ages, &mut floors, 0).await.unwrap();
    assert_eq!(holding, Some(Held::Archive), "a segment whose seqs the bucket holds only in a tail stays");
    assert!(window.restore.read_back(&unfilled).await.is_err(), "the bucket holds no range for it");
    fixture.node.stop().await;
}

/// The window counts the log's own stamps, counted back from the newest segment's first command: a log stamped
/// long before the host's clock keeps every segment within the window of its own time, and no snapshot leaves none,
/// which pages nothing however long it lasts: the node's daily snapshots are its own to keep, and a rotation member
/// holds only those of its turns.
#[tokio::test]
async fn the_window_counts_sequenced_time_and_no_snapshot_keeps_every_segment() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let fixture = Fixture::start("window-time", 90).await;
    let ranges = fixture.archive_all();
    assert!(START + 90 * MINUTE_MS < crate::clock::now_ms() - 86_400_000, "the log is stamped long before the host");
    let (mut window, _archived) = fixture.window((120, 30), ranges);
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert_eq!((pass.held, pass.removed.len()), (Some(Held::Window), 0), "all within two hours of the log's time");
    let (mut window, _archived) = fixture.window((30, 30), ranges);
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert_eq!((pass.held, pass.removed.len()), (Some(Held::Snapshot), 0), "no daily snapshot, nothing leaves");
    fixture.clock.advance(HELD_ALERT);
    fixture.ticks(10).await;
    let (mut window, _archived) = fixture.window((30, 30), fixture.archive_all());
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert_eq!(pass.held, Some(Held::Snapshot));
    assert!(window.metrics.archive.window.held_seconds.get() >= HELD_ALERT.as_secs_f64());
    assert!(!drained(&mut alerts, "log_window_held"), "a snapshot's hold pages nothing");
    fixture.node.stop().await;
}

/// The history floor binds where it is before the newest snapshot a start may go on from: none stamped before the
/// history's retention keeps every segment, and the newest that is keeps every segment from its checkpoint.
#[tokio::test]
async fn the_history_keeps_the_segments_from_the_snapshot_at_its_retention() {
    let fixture = Fixture::start("window-history", 160).await;
    let ranges = fixture.archive_all();
    for minutes in [100, 120, 140] {
        fixture.snapshot_at(minutes);
    }
    // The start's floor is the snapshot at minute 140; the history, keeping 90 minutes, has none before its retention.
    let (mut window, _archived) = fixture.window((90, 90), ranges);
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert_eq!((pass.held, pass.removed.len()), (Some(Held::History), 0), "{pass:?}");
    // One at minute 20 whose file is shorter than its listing says, or as long with other bytes, does not count: a
    // re-derivation could not load it.
    let early = fixture.snapshot_at(20);
    let listed = crate::daily_snapshots::read_manifest(&fixture.snapshots).unwrap();
    let file = fixture.snapshots.join(&listed.iter().find(|listing| listing.seq == early).unwrap().file);
    let whole = std::fs::read(&file).unwrap();
    let mut other = whole.clone();
    other[0] ^= 1;
    for damaged in [&whole[..whole.len() - 1], &other[..]] {
        std::fs::write(&file, damaged).unwrap();
        let pass = window.pass(&fixture.view()).await.unwrap();
        assert_eq!((pass.held, pass.removed.len()), (Some(Held::History), 0), "{pass:?}");
    }
    // Whole, the history's floor is there, before the start's.
    std::fs::write(&file, whole).unwrap();
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert_eq!(pass.held, Some(Held::History), "{pass:?}");
    assert!(!pass.removed.is_empty());
    assert!(fixture.view().segments.iter().all(|listed| held(&fixture.dir, listed) == (listed.last_seq >= early)));
    fixture.node.stop().await;
}

/// The window keeps a segment a reader holds, and lets it go once the reader moves past it or is dropped.
#[tokio::test]
async fn a_segment_a_reader_holds_stays_until_the_reader_moves_on() {
    let fixture = Fixture::start("window-reader", 150).await;
    let ranges = fixture.archive_all();
    fixture.snapshot_at(100);
    let first = fixture.view().segments[2].clone();
    let mut reader =
        LogReader::open(&fixture.dir, Offset::start_of(first.first_seq), fixture.node.manifest.clone()).unwrap();
    reader.next_entry().unwrap().expect("an entry");
    let (mut window, _archived) = fixture.window((30, 30), ranges);
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert_eq!((pass.held, pass.removed.len()), (Some(Held::Reader), 2), "the two before the reader's left: {pass:?}");
    assert!(held(&fixture.dir, &first));
    drop(reader);
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert!(!held(&fixture.dir, &first), "the dropped reader's segment left: {pass:?}");
    fixture.node.stop().await;
}

/// A bucket that is down, or that gives back other bytes than a segment's listing names, removes nothing, and the
/// window grows past `HELD_ALERT` into `log_window_held`; once the bucket serves the segment, it leaves.
#[tokio::test]
async fn a_bucket_that_does_not_give_a_segment_back_keeps_it_and_the_held_alert_fires() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let fixture = Fixture::start("window-read-back", 150).await;
    let ranges = fixture.archive_all();
    fixture.snapshot_at(60);
    fixture.snapshot_at(140);
    fixture.bucket.with(|b| b.down = true);
    let (mut window, archived) = fixture.window((30, 30), ranges);
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert_eq!((pass.held, pass.removed.len()), (Some(Held::ReadBack), 0), "{pass:?}");
    assert!(!drained(&mut alerts, "log_window_held"), "within HELD_ALERT of the window, nothing pages");
    // Two days on, past the window by more than HELD_ALERT, the alert names why the segment stays.
    fixture.clock.advance(HELD_ALERT);
    fixture.ticks(10).await;
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert_eq!(pass.held, Some(Held::ReadBack));
    let alert = named(&mut alerts, "log_window_held").await;
    assert!(alert["message"].contains("does not give back"), "{alert:?}");
    // The first pass resolved what it found well, as a start does: only the resolution after the raise counts.
    while resolved.try_recv().is_ok() {}
    assert!(window.metrics.archive.window.held_seconds.get() >= HELD_ALERT.as_secs_f64());
    fixture.bucket.with(|b| b.down = false);
    let key = crate::archive::range_key(&crate::archive::stand_in::test_log(), 0);
    fixture.bucket.with(|b| b.served.insert(key, b"other bytes".to_vec()));
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert_eq!((pass.held, pass.removed.len()), (Some(Held::ReadBack), 0), "other bytes than the listing's");
    fixture.bucket.with(|b| b.served.clear());
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert!(!pass.removed.is_empty(), "{pass:?}");
    // With an hour of the log and its snapshots after the gap, the oldest segment kept is within the window again.
    fixture.ticks(60).await;
    let later = 150 + HELD_ALERT.as_secs() as Millis / 60 + 10;
    for minutes in [later + 25, later + 40, later + 50] {
        fixture.snapshot_at(minutes);
    }
    archived.send_replace(Some(fixture.archive_all()));
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert!(matches!(pass.held, Some(Held::Window | Held::History)), "within the window, or its day past: {pass:?}");
    named(&mut resolved, "log_window_held").await;
    fixture.node.stop().await;
}

/// Passes that keep failing, here over a newest segment whose bytes differ from its listing, raise
/// `log_window_failing` once, and the next good pass resolves it.
#[tokio::test]
async fn passes_that_keep_failing_raise_the_failing_alert_until_one_succeeds() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let fixture = Fixture::start("window-failing", 30).await;
    let ranges = fixture.archive_all();
    let newest = fixture.view().segments.last().cloned().unwrap();
    let path = fixture.dir.join(&newest.file);
    let good = std::fs::read(&path).unwrap();
    let mut bad = good.clone();
    bad[1] ^= 1;
    std::fs::write(&path, &bad).unwrap();
    let (window, _archived) = fixture.window((30, 30), ranges);
    let (writers, sources) = watch::channel(Some(fixture.node.manifest.clone()));
    let running = tokio::spawn(Window { sources, ..window }.run());
    named(&mut alerts, "log_window_failing").await;
    assert!(!drained(&mut alerts, "log_window_failing"), "raised once");
    std::fs::write(&path, &good).unwrap();
    named(&mut resolved, "log_window_failing").await;
    running.abort();
    drop(writers);
    fixture.node.stop().await;
}

/// A start goes on from no daily snapshot after the newest whose frozen marks copy the node holds, nor after the
/// history index's last command, as a member's start does, so the start's floor is the newest at or before both.
#[tokio::test]
async fn the_starts_floor_is_bound_by_the_frozen_marks_and_the_history_index() {
    let fixture = Fixture::start("window-floors", 160).await;
    let seqs: Vec<u64> = [60, 100, 140].into_iter().map(|minutes| fixture.snapshot_at(minutes)).collect();
    let floor = |history: &Path| {
        let history =
            HistorySection { dir: history.to_owned(), retention: crate::test_support::RETENTION, marks: None };
        Floors::read(&fixture.snapshots, &history, u64::MAX, fixture.now(), &mut Checked::default()).snapshot
    };
    std::fs::create_dir_all(&fixture.history).unwrap();
    assert_eq!(floor(&fixture.history), Some(seqs[2]), "neither binds: the newest");
    let framed = crate::history::marks::store::frozen_path(&fixture.history, Offset { seq: seqs[1], index: 1 });
    std::fs::write(framed, b"").unwrap();
    assert_eq!(floor(&fixture.history), Some(seqs[1]), "the newest whose frozen marks copy the node holds");
    // An index whose last command is the one at the oldest snapshot's checkpoint.
    let other = scratch_dir("window-floors-index");
    let (log, index) = (other.join("log"), other.join("history"));
    let node = TestSequencer::start(&log, 256);
    while node.next_seq().await <= seqs[0] {
        node.submit(Command::Tick).await;
    }
    let history = crate::test_support::TestHistory::start(&node, &log, &index).await.unwrap();
    node.stop().await;
    history.finish().await.unwrap();
    assert_eq!(floor(&index), Some(seqs[0]), "the newest at or before the index's last command");
    fixture.node.stop().await;
}

/// A directory that does not hold the newest segment its writer names, here one whose writer appends to none and that
/// lost its newest completed segment, fails the pass, which touches nothing of it, so failing passes page.
#[tokio::test]
async fn a_directory_without_the_newest_segment_its_writer_names_fails_the_pass() {
    let fixture = Fixture::start("window-newest-lost", 60).await;
    let ranges = fixture.archive_all();
    let mut view = fixture.view();
    view.active = None;
    let newest = view.segments.last().cloned().unwrap();
    std::fs::remove_file(fixture.dir.join(&newest.file)).unwrap();
    let (mut window, _archived) = fixture.window((600, 30), ranges);
    let failed = window.pass(&view).await.expect_err("the directory lacks the newest segment");
    assert!(format!("{failed:#}").contains("does not hold the newest segment"), "{failed:#}");
    assert!(!held(&fixture.dir, &newest), "nothing is written into it");
    fixture.node.stop().await;
}

/// A start's window knows none of its alerts' states, which the alert router keeps open across a restart: its first
/// pass over a log it finds well resolves each, and a later raise pages again.
#[tokio::test]
async fn a_starting_window_resolves_the_alerts_an_earlier_process_left_open() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let fixture = Fixture::start("window-start-alerts", 30).await;
    let (window, _archived) = fixture.window((600, 30), fixture.archive_all());
    let (writers, sources) = watch::channel(Some(fixture.node.manifest.clone()));
    let running = tokio::spawn(Window { sources, ..window }.run());
    for name in ["log_segment_missing", "log_window_held", "log_window_failing"] {
        named(&mut resolved, name).await;
    }
    assert!(!drained(&mut resolved, "log_window_held"), "resolved once");
    assert!(!drained(&mut alerts, "log_window_held") && !drained(&mut alerts, "log_segment_missing"));
    running.abort();
    drop(writers);
    fixture.node.stop().await;
}

/// The next alert, or resolution, named `name`.
pub(super) async fn named(alerts: &mut UnboundedReceiver<Alert>, name: &str) -> Alert {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let alert = alerts.recv().await.expect("alerts collected");
            if alert.get("alert").or_else(|| alert.get("alert_resolved")).is_some_and(|raised| raised == name) {
                return alert;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {name} in time"))
}

/// Whether any alert, or resolution, collected so far is named `name`.
pub(super) fn drained(alerts: &mut UnboundedReceiver<Alert>, name: &str) -> bool {
    std::iter::from_fn(|| alerts.try_recv().ok())
        .any(|alert| alert.get("alert").or_else(|| alert.get("alert_resolved")).is_some_and(|raised| raised == name))
}
