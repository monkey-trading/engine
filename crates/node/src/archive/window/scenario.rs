//! The log's local window's dev scenario (`scripts/dev-log-window.sh`), on accelerated sequenced time with isolated
//! temporary state: a dev node with a window of hours, which only a development build takes, writes days of flow and
//! a daily snapshot at each UTC day's first checkpoint, uploads its ranges to the bucket's loopback stand-in, and
//! keeps on its host the window and no more than a day past it, through a day's outage of the bucket, a segment its host
//! loses before the bucket holds it, and a restart of its window; the node, and a member's replica on a copy trimmed
//! as its own window would, each start from the newest snapshot; and the whole log still replays from genesis from the
//! node's archive, which redirects a segment its host no longer holds to the bucket.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engine_core::command::Command;
use engine_core::types::{AccountId, Millis};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

use crate::archive::read::origin::{bucket_of, copy_log};
use crate::archive::stand_in::{StandIn, Vault, test_log};
use crate::archive::window::{RestoreSources, Window};
use crate::archive::{Origins, RANGE_SEQS, Uploader, download};
use crate::clock::test_clock::TestClock;
use crate::config::HistorySection;
use crate::daily_snapshots::{Bounds, Listed, MANIFEST as SNAPSHOTS, NoRoots, read_manifest, snapshot_start_against};
use crate::log::{CheckpointDomain, LogEntry, Manifest, ReplayWorkers, SegmentInfo};
use crate::metrics::Metrics;
use crate::sequencer::{Request, Sequencer, replay_path_in_domain};
use crate::standby::archive::bootstrap_archive;
use crate::tee::EnclaveKey;
use crate::test_support::{
    SEGMENT_COMMANDS, TestSequencer, capture_alerts_and_resolutions, sample_around, scratch_dir,
};

/// Six hours before a UTC midnight.
const START: Millis = 1_792_540_800_000 - 6 * HOUR_MS;
const HOUR_MS: Millis = 3_600_000;
const DAY_MS: Millis = 24 * HOUR_MS;
/// The window, and the history's retention, both six hours.
const WINDOW_MS: Millis = 6 * HOUR_MS;
/// The flow: a tick every ten minutes of sequenced time.
const STEP: Duration = Duration::from_secs(600);
/// How long the scenario waits for any one step.
const WAIT: Duration = Duration::from_secs(120);
/// The test sequencer's deployment.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// The node: its sequencer on a test clock, and its uploader and window, each on its own task.
struct Node {
    run: PathBuf,
    dir: PathBuf,
    snapshots: PathBuf,
    clock: Arc<TestClock>,
    sequencer: TestSequencer,
    bucket: StandIn,
    metrics: Arc<Metrics>,
    tasks: Vec<JoinHandle<anyhow::Result<()>>>,
}

#[tokio::test]
#[ignore = "a dev scenario: scripts/dev-log-window.sh runs it"]
async fn dev_scenario_log_window() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let mut node = Node::start().await;

    println!("==> 1. three and a half days of flow: the host keeps the window and the newest snapshot's age");
    node.write(84).await;
    let held = node.settled().await;
    println!("    {held}");
    node.write(24).await;
    let later = node.settled().await;
    println!("    a day on, {later}");

    println!(
        "==> 2. the bucket goes down for a day: nothing leaves, the host's log grows and archive_upload_lagging fires"
    );
    node.bucket.with(|b| b.down = true);
    // Once a pass begun after the outage has ended, every removal a read-back before it allowed is done.
    node.passed().await;
    let (removed, first) = (node.metrics.archive.window.removed.get(), node.first_local());
    // Across a UTC midnight: the new daily snapshot moves the floors on, and what they let go needs a read-back.
    node.write(24).await;
    expect(&mut alerts, "archive_upload_lagging").await;
    node.passed().await;
    assert_eq!(node.metrics.archive.window.removed.get(), removed, "nothing leaves while the bucket is down");
    assert_eq!(node.first_local(), first, "the oldest segment stays");
    node.bucket.with(|b| b.down = false);
    expect(&mut resolved, "archive_upload_lagging").await;
    let resumed = node.settled().await;
    assert!(node.metrics.archive.window.removed.get() > removed, "the window moves on once the bucket is back");
    println!("    a day kept through the outage; back up, {resumed}");

    println!(
        "==> 3. the host loses a segment the bucket does not hold: log_segment_missing fires, and a member's copy"
    );
    println!("       uploads its ranges, from which the node puts the segment back");
    node.stop_tasks();
    let (confirmed, trimmed) = (node.metrics.archive.confirmed_ranges.get() as u64, node.first_local());
    node.write(3).await;
    let view = node.view();
    let member = node.run.join("member");
    copy_log(&node.dir, &member);
    let lost = view.segments.iter().find(|listed| listed.first_seq >= confirmed * RANGE_SEQS).cloned().unwrap();
    assert!(view.segments.last().is_some_and(|newest| newest.first_seq > lost.first_seq), "not the newest");
    let bytes = std::fs::read(node.dir.join(&lost.file)).unwrap();
    std::fs::remove_file(node.dir.join(&lost.file)).unwrap();
    node.start_tasks();
    let alert = expect(&mut alerts, "log_segment_missing").await;
    assert_eq!(alert["segment"], lost.file);
    let (uploader, sources) = uploader(&node.bucket, &member, &Arc::new(Metrics::new()));
    sources.send_replace(Some(view_from_first_local(&member)));
    let member_uploads = tokio::spawn(uploader.run());
    let resolution = expect(&mut resolved, "log_segment_missing").await;
    assert_eq!(resolution["segment"], lost.file);
    assert_eq!(std::fs::read(node.dir.join(&lost.file)).unwrap(), bytes, "put back as its listing names it");
    member_uploads.abort();
    assert!(node.first_local() >= trimmed, "the restarted window put back nothing the bucket holds");
    println!("    {} put back from the ranges the member's copy uploaded", lost.file);

    println!("==> 4. the node, and a member whose copy its window trimmed, each start from the newest snapshot");
    let manifest = node.sequencer.manifest.clone();
    let (dir, snapshots, run) = (node.dir.clone(), node.snapshots.clone(), node.run.clone());
    let bucket = node.stop().await;
    let first = crate::log::list_segments(&dir).unwrap()[0].0;
    let newest = start_from_newest(&snapshots, &dir).await;
    assert!(first > 0 && first <= newest.seq, "the host holds the log from the snapshot's segment, seq {first}, on");
    let url = serve_archive(manifest, &dir, &bucket).await;
    let removed = crate::log::segment_name(0);
    let answer = bucket.client.get(format!("{url}/v1/archive/{removed}")).send().await.unwrap();
    assert_eq!(answer.status(), 308, "the node redirects a segment it no longer holds to the bucket");
    let copy = run.join("member-copy");
    let origins = Origins { bucket: Some(bucket_of(&bucket)), nodes: vec![url.clone()] };
    bootstrap_archive(&bucket.client, &origins, &copy).await.unwrap();
    let member_snapshots = run.join("member-snapshots");
    take_snapshot(&snapshots, &member_snapshots, &newest);
    let trimmed = trim_before(&copy, newest.seq);
    let replays = member_replica(&member_snapshots, &copy, &dir).await;
    assert_eq!(replays, 0, "the member's replica replayed nothing whole");
    println!(
        "    both went on from the snapshot at seq {}, the node's log from seq {first} and the member's copy without its \
         {trimmed} segments before it",
        newest.seq
    );

    println!(
        "==> 5. engine replay --log <the node's URL> replays the archive from genesis, each checkpoint the node's"
    );
    let downloaded = run.join("downloaded");
    download(&bucket.client, &url.parse().unwrap(), &downloaded).await.unwrap();
    // The archive serves completed segments: every command of them from genesis, each checkpoint's root checked.
    let replayed = replay_path_in_domain(&downloaded, None).unwrap();
    let completed = Manifest::load(&dir).unwrap().unlisted_from().unwrap();
    assert_eq!(replayed.commands, completed, "every command of the completed segments from genesis");
    let last = crate::log::last_checkpoint(&downloaded).unwrap().expect("a checkpoint");
    assert_eq!(checkpoint_at(&dir, last.seq), Some(last.clone()), "the node's own checkpoint at seq {}", last.seq);
    println!("    {} commands from genesis, to the node's checkpoint at seq {}", replayed.commands, last.seq);
    println!("PASS: the host keeps the window and the newest snapshot, and the archive keeps the whole log");
}

impl Node {
    /// A dev node publishing daily snapshots, with its uploader and window running.
    async fn start() -> Self {
        let run = scratch_dir("log-window-scenario");
        let (dir, snapshots) = (run.join("log"), run.join("snapshots"));
        let clock = Arc::new(TestClock::at(START));
        let mut sequencer = TestSequencer::start_publishing(&dir, 1024, clock.clone(), &snapshots);
        // No marks task: each daily snapshot's freeze is dropped, so it goes without the chart history's anchors.
        let mut marks = sequencer.take_marks();
        tokio::spawn(async move { while marks.recv().await.is_some() {} });
        let (bucket, metrics) = (StandIn::start().await, Arc::new(Metrics::new()));
        let mut node = Self { run, dir, snapshots, clock, sequencer, bucket, metrics, tasks: Vec::new() };
        node.start_tasks();
        node
    }

    /// Starts the uploader and the window on the node's log.
    fn start_tasks(&mut self) {
        let (uploader, sources) = uploader(&self.bucket, &self.dir, &self.metrics);
        let history = HistorySection {
            dir: self.run.join("history"),
            retention: Duration::from_millis(WINDOW_MS as u64),
            marks: None,
        };
        let restore =
            RestoreSources { http: self.bucket.client.clone(), bucket: bucket_of(&self.bucket), nodes: vec![] };
        let window = Window::new(self.dir.clone(), WINDOW_MS, self.snapshots.clone(), history, restore).following(
            sources.subscribe(),
            uploader.archived(),
            self.metrics.clone(),
        );
        sources.send_replace(Some(self.sequencer.manifest.clone()));
        self.tasks.push(tokio::spawn(async move {
            let _sources = sources;
            uploader.run().await
        }));
        self.tasks.push(tokio::spawn(window.run()));
    }

    /// Stops the uploader and the window.
    fn stop_tasks(&mut self) {
        self.tasks.drain(..).for_each(|task| task.abort());
    }

    /// Writes `hours` of flow, a tick every `STEP`.
    async fn write(&self, hours: u64) {
        for _ in 0..hours * 6 {
            self.clock.advance(STEP);
            self.sequencer.tx.send(Request::TimeSample(sample_around(self.clock.as_ref()))).await.unwrap();
            self.sequencer.submit(Command::Tick).await;
        }
    }

    /// The writer's view of the log.
    fn view(&self) -> Manifest {
        self.sequencer.manifest.borrow().clone()
    }

    /// The first seq of the oldest segment on the host.
    fn first_local(&self) -> u64 {
        crate::log::list_segments(&self.dir).unwrap().first().map(|(seq, _)| *seq).unwrap()
    }

    /// Waits until the window has removed every segment that may leave, then checks that the host holds the window
    /// and no more than a day past it, the daily snapshot at the start of the history's retention, or the newest,
    /// keeping the segments from its checkpoint, and says what it holds.
    async fn settled(&self) -> String {
        let started = Instant::now();
        loop {
            let (view, first) = (self.view(), self.first_local());
            let newest = view.segments.last().cloned().unwrap();
            let now = first_stamp(&self.dir, &newest);
            let oldest = view.segments.iter().find(|listed| listed.first_seq == first).cloned().unwrap();
            let span = now - first_stamp(&self.dir, &oldest);
            let snapshot = read_manifest(&self.snapshots).unwrap().last().map_or(0, |listing| now - listing.ts);
            if span <= WINDOW_MS + DAY_MS + 2 * STEP.as_millis() as Millis {
                assert!(span >= WINDOW_MS, "the host keeps the window: {span} ms");
                let bytes = self.metrics.archive.window.local_bytes.get();
                assert!(bytes > 0, "the log's bytes on the host are measured");
                return format!(
                    "the host holds {:.1} hours of the log from seq {first}, {bytes} bytes; the newest snapshot is {:.1} \
                     hours old",
                    span as f64 / HOUR_MS as f64,
                    snapshot as f64 / HOUR_MS as f64
                );
            }
            assert!(started.elapsed() < WAIT, "the window did not settle: the host holds {span} ms of the log");
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits until a pass of the window begun after this call has ended: the one under way then ends first.
    async fn passed(&self) {
        let (passes, started) = (self.metrics.archive.window.passes.get(), Instant::now());
        while self.metrics.archive.window.passes.get() < passes + 2 {
            assert!(started.elapsed() < WAIT, "the window did not pass");
            sleep(Duration::from_millis(10)).await;
        }
    }

    /// Stops the node's tasks and its sequencer, and hands back the bucket.
    async fn stop(mut self) -> StandIn {
        self.stop_tasks();
        self.sequencer.stop().await;
        self.bucket
    }
}

/// The manifest of the copy in `dir`, which lacks the segments the window removed, as its writer publishes it.
fn view_from_first_local(dir: &Path) -> watch::Receiver<Manifest> {
    let first = crate::log::list_segments(dir).unwrap().first().map(|(seq, _)| *seq).unwrap();
    let log = crate::log::SegmentedLog::open_from(dir, crate::log::Offset::start_of(first), |_| Ok(())).unwrap();
    watch::channel(log.manifest_view()).1
}

/// An uploader of the log in `dir` to `bucket`, reading no vault, as under mock custody, and its sources.
fn uploader(bucket: &StandIn, dir: &Path, metrics: &Arc<Metrics>) -> (Uploader<Vault>, crate::archive::Sources) {
    let lag = Duration::from_millis(500);
    Uploader::new((bucket.store(), test_log()), dir.to_owned(), lag, metrics.clone(), None)
}

/// The stamp of `listed`'s first command, from its file in `dir`.
fn first_stamp(dir: &Path, listed: &SegmentInfo) -> Millis {
    let mut stamps = crate::archive::window::Stamps::default();
    stamps.of(dir, listed).unwrap()
}

/// Starts from the newest daily snapshot in `snapshots` and the log in `dir` after it, which lacks the segments before
/// it, and returns its listing.
async fn start_from_newest(snapshots: &Path, dir: &Path) -> Listed {
    let newest = read_manifest(snapshots).unwrap().pop().expect("a snapshot");
    let signers = [AccountId(EnclaveKey::test_key().address())].into();
    let bounds = Bounds { finalized: u64::MAX, before: None, keep_from: None };
    let start = snapshot_start_against(None, (snapshots, dir), (DOMAIN, &signers), bounds, &NoRoots)
        .await
        .expect("the newest snapshot is usable");
    assert_eq!(start.snapshot.offset.seq, newest.seq, "the start takes the newest snapshot");
    let trust = (Some(DOMAIN), signers.first().copied(), None);
    let workers = ReplayWorkers::available();
    let observe = |_: &LogEntry| {};
    let recovered =
        Sequencer::read_log_starting(start, dir, trust, Default::default(), observe, None, None, None, workers)
            .expect("the start goes on from the snapshot");
    assert_eq!(recovered.snapshot_sequence(), Some(newest.seq));
    newest
}

/// Removes from the copy in `dir` the segments that end before `seq`, as its own window would, and returns how many.
fn trim_before(dir: &Path, seq: u64) -> usize {
    let listed = Manifest::load(dir).unwrap().segments;
    let before: Vec<&SegmentInfo> = listed.iter().filter(|segment| segment.last_seq < seq).collect();
    assert!(!before.is_empty(), "the copy holds segments before seq {seq}");
    before.iter().for_each(|segment| std::fs::remove_file(dir.join(&segment.file)).unwrap());
    before.len()
}

/// Builds a member's replica of the copy in `copy` from the newest daily snapshot in `snapshots`, as a member opens
/// its copy, checks that it ends at the copy's last checkpoint, as the node's log in `node` holds it, and returns how
/// many whole replays it ran.
async fn member_replica(snapshots: &Path, copy: &Path, node: &Path) -> u64 {
    let signer = AccountId(EnclaveKey::test_key().address());
    let signers: std::collections::BTreeSet<AccountId> = [signer].into();
    let bounds = Bounds { finalized: u64::MAX, before: None, keep_from: None };
    let signed = snapshot_start_against(None, (snapshots, copy), (DOMAIN, &signers), bounds, &NoRoots)
        .await
        .expect("the member's snapshot is usable");
    let workers = ReplayWorkers::available();
    let records = (None, None, None, None);
    let follower = crate::standby::Follower::open_pinned(
        copy,
        SEGMENT_COMMANDS,
        workers,
        DOMAIN,
        signer,
        signers,
        records,
        (None, &[]),
        Some(signed),
        |_| Ok(()),
    )
    .expect("the member builds its replica from the snapshot");
    // The archive serves completed segments, so the copy ends at the last checkpoint of the node's completed ones.
    let last = crate::log::last_checkpoint(copy).unwrap().expect("a checkpoint");
    let at = |checkpoint: &crate::log::Checkpoint| (checkpoint.seq, checkpoint.root.clone());
    assert_eq!(follower.replica().last_checkpoint().map(at), Some(at(&last)), "the replica ends where its copy does");
    assert_eq!(checkpoint_at(node, last.seq).as_ref().map(at), Some(at(&last)), "the node wrote that checkpoint");
    follower.replays()
}

/// The checkpoint at `seq` in the log in `dir`, read from its first segment on the host on.
fn checkpoint_at(dir: &Path, seq: u64) -> Option<crate::log::Checkpoint> {
    let (mut found, first) = (None, crate::log::list_segments(dir).unwrap()[0].0);
    crate::log::SegmentedLog::open_from(dir, crate::log::Offset::start_of(first), |entry| {
        if let LogEntry::Checkpoint(checkpoint) = entry
            && checkpoint.seq == seq
        {
            found = Some(checkpoint);
        }
        Ok(())
    })
    .unwrap();
    found
}

/// Copies `listing`'s snapshot from `from` into `to`, listed, as a member fetches one.
fn take_snapshot(from: &Path, to: &Path, listing: &Listed) {
    std::fs::create_dir_all(to).unwrap();
    std::fs::copy(from.join(&listing.file), to.join(&listing.file)).unwrap();
    std::fs::write(to.join(SNAPSHOTS), format!("{}\n", serde_json::to_string(listing).unwrap())).unwrap();
}

/// Serves the archive of the log in `dir`, whose writer published `manifest`, from the node's own route, a segment
/// the host no longer holds redirected to `bucket`.
async fn serve_archive(manifest: watch::Receiver<Manifest>, dir: &Path, bucket: &StandIn) -> String {
    let (mut state, _requests) = crate::api::tests::api_state(true);
    (state.log_dir, state.manifest, state.archive_objects) = (dir.to_owned(), manifest, Some(bucket_of(bucket)));
    // A whole log of four-command segments is more requests than the dev config's per-client limit lets through.
    let mut api = crate::test_support::dev_config().api;
    let many = std::num::NonZeroU32::new(1_000_000).expect("non-zero");
    api.per_ip = crate::config::RateLimit { per_second: many, burst: many };
    state.limits = Arc::new(crate::limits::ApiLimits::new(&api));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = crate::api::router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}")
}

/// The next alert, or resolution, named `name`.
async fn expect(
    alerts: &mut tokio::sync::mpsc::UnboundedReceiver<crate::test_support::Alert>,
    name: &str,
) -> crate::test_support::Alert {
    tokio::time::timeout(WAIT, async {
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
