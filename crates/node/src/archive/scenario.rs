//! The log archive's dev scenario (`scripts/dev-log-archive.sh`): a dev node uploads its log's ranges to a loopback
//! stand-in for the bucket as a stand-in vault finalizes them, through an outage, a member's differently cut copy, a
//! copy the vault's history superseded and a restart, until a read-back differs.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engine_core::command::Command;
use tokio::time::{Instant, sleep, timeout};

use crate::archive::range::tests::resegmented;
use crate::archive::stand_in::{StandIn, Vault, checkpoint_past, checkpoints, test_log};
use crate::archive::{RANGE_SEQS, Uploader, holds, range_bytes, range_key, whole_ranges};
use crate::log::{LogEntry, Manifest};
use crate::metrics::Metrics;
use crate::test_support::{
    Alert, TestSequencer, capture_alerts_and_resolutions, log_entries, published_manifest, scratch_dir,
};

/// How long the scenario waits for any one step.
const WAIT: Duration = Duration::from_secs(60);
/// A root no log of the scenario's has.
const OTHER_ROOT: &str = "0xabababababababababababababababababababababababababababababababab";

#[tokio::test]
async fn dev_scenario_log_archive() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let (bucket, dir, vault) = (StandIn::start().await, scratch_dir("archive-scenario"), Vault::default());
    let lag = Duration::from_millis(500);

    println!("==> 1. a dev node's whole ranges go up compressed once the vault finalizes a checkpoint past them");
    let node = TestSequencer::start(&dir, 256);
    ticks_until(&node, 4).await;
    let (uploader, sources, metrics) = uploading(&bucket, &dir, lag, &vault);
    sources.send_replace(Some(node.manifest.clone()));
    let mut running = tokio::spawn(uploader.run());
    until("the whole ranges counted", || metrics.archive.pending_ranges.get() >= 4).await;
    sleep(Duration::from_millis(200)).await;
    assert_eq!(bucket.with(|b| b.created), 0, "nothing goes up before the vault finalizes a checkpoint");
    let first = vault.finalize_past(&node, &dir, 3).await;
    until("the ranges below it confirmed", || metrics.archive.confirmed_ranges.get() as u64 == first / RANGE_SEQS)
        .await;
    sleep(Duration::from_millis(200)).await;
    assert_eq!(bucket.with(|b| b.created), first / RANGE_SEQS, "no range past the vault's checkpoint goes up");
    let whole = finalize_all(&node, &dir, &vault, &metrics).await;
    check_objects(&bucket, &dir, &node.manifest.borrow().clone(), whole);
    let (raw, stored) = (metrics.archive.confirmed_bytes.get(), metrics.archive.stored_bytes.get());
    println!("    {} ranges below seq {first} first, then {whole}; {raw} bytes stored as {stored}", first / RANGE_SEQS);

    println!("==> 2. the bucket goes down: sequencing goes on, the backlog grows and archive_upload_lagging fires");
    quiet(&mut alerts, &mut resolved).await;
    bucket.with(|b| b.down = true);
    ticks_until(&node, whole + 4).await;
    let past = finalize_all_after(&node, &dir, &vault, whole + 4).await;
    let alert = expect(&mut alerts, "archive_upload_lagging").await;
    assert!(alert["message"].contains("whole ranges"), "{alert:?}");
    until("the outage's ranges counted", || metrics.archive.pending_ranges.get() >= 4).await;
    let pending = metrics.archive.pending_ranges.get();
    // The lag alert and the count of waiting ranges come from the backlog, not from an attempt: the uploader
    // reads the vault's new checkpoint on its own poll, so its first failed upload can come after both.
    until("an upload to fail", || metrics.archive.failures.get() > 0).await;
    bucket.with(|b| b.down = false);
    expect(&mut resolved, "archive_upload_lagging").await;
    until("the outage's ranges confirmed", || metrics.archive.confirmed_ranges.get() as u64 >= past).await;
    let whole = past;
    println!("    {pending} ranges waited through the outage; caught up through {whole}");

    println!(
        "==> 3. the node's uploader stops, and a member's copy, cut into other segments, uploads the ranges after"
    );
    running.abort();
    ticks_until(&node, whole + 2).await;
    let target = finalize_all_after(&node, &dir, &vault, whole + 2).await;
    let copy = scratch_dir("archive-scenario-copy");
    copy_of(&node, &dir, &copy, target).await;
    let (held, created) = bucket.with(|b| (b.objects.clone(), b.created));
    let (member, member_sources, member_metrics) = uploading(&bucket, &copy, lag, &vault);
    member_sources.send_replace(Some(published_manifest(&copy)));
    let member_running = tokio::spawn(member.run());
    let found = || member_metrics.archive.confirmed_ranges.get() as u64 >= target;
    until("the member's ranges confirmed", found).await;
    member_running.abort();
    let objects = bucket.with(|b| b.objects.clone());
    assert!(held.iter().all(|(key, object)| objects.get(key) == Some(object)), "every earlier object is as it was");
    assert_eq!(bucket.with(|b| b.created) - created, target - whole, "the member uploads the ranges the bucket lacks");
    check_objects(&bucket, &dir, &node.manifest.borrow().clone(), target);
    println!("    the member uploaded ranges {whole} to {} from its own cut, the bytes the node builds", target - 1);

    println!("==> 4. a copy the vault's history superseded uploads nothing past it, and archive_copy_superseded fires");
    ticks_until(&node, target + 2).await;
    let (seq, _) = checkpoint_past(&node, &dir, target).await;
    vault.finalize(seq, OTHER_ROOT);
    let (stale, stale_sources, stale_metrics) = uploading(&bucket, &dir, lag, &vault);
    stale_sources.send_replace(Some(node.manifest.clone()));
    let stale_running = tokio::spawn(stale.run());
    let alert = expect(&mut alerts, "archive_copy_superseded").await;
    assert!(alert["message"].contains(&format!("seq {seq} ")), "{alert:?}");
    sleep(Duration::from_millis(200)).await;
    stale_running.abort();
    assert_eq!(stale_metrics.archive.objects_created.get(), 0, "nothing past the vault's other history goes up");
    let ranges = bucket.with(|b| b.objects.keys().filter(|key| !key.contains("/tail/")).count() as u64);
    assert_eq!(ranges, target);
    println!("    the vault's checkpoint at seq {seq} names another root, and the copy uploaded nothing");

    println!("==> 5. the node restarts, and its uploader resumes at the first missing range");
    node.stop().await;
    let node = TestSequencer::start(&dir, 256);
    ticks_until(&node, target + 2).await;
    vault.finalize_past(&node, &dir, target).await;
    let (asked, read) = bucket.with(|b| (b.asked, b.read));
    let (uploader, sources, metrics) = uploading(&bucket, &dir, lag, &vault);
    sources.send_replace(Some(node.manifest.clone()));
    running = tokio::spawn(uploader.run());
    let now = finalize_all(&node, &dir, &vault, &metrics).await;
    let (asked, read) = bucket.with(|b| (b.asked - asked, b.read - read));
    assert!(asked <= u64::from(u64::BITS - now.leading_zeros()) + 1, "{asked} existence checks for {now}");
    assert_eq!(read, 1 + now - target, "the range before the new ones, and each new one, read back");
    check_objects(&bucket, &dir, &node.manifest.borrow().clone(), now);
    println!("    resumed with {asked} existence checks and {read} reads, and confirmed through {now}");

    println!("==> 6. the bucket serves other bytes for the next range: the Warning fires, and the uploads stop there");
    let key = range_key(&test_log(), now);
    bucket.with(|b| b.served.insert(key.clone(), b"other bytes".to_vec()));
    finalize_all_after(&node, &dir, &vault, now + 1).await;
    let alert = expect(&mut alerts, "archive_object_mismatch").await;
    assert_eq!(alert["object"], key);
    let created = bucket.with(|b| b.created);
    finalize_all_after(&node, &dir, &vault, now + 4).await;
    until("the ranges after it counted", || metrics.archive.pending_ranges.get() >= 4).await;
    sleep(Duration::from_millis(200)).await;
    assert_eq!(bucket.with(|b| b.created), created, "no range goes up past the mismatch");
    assert_eq!(metrics.archive.confirmed_ranges.get() as u64, now);
    println!("    {key} reported, and nothing past it went up while the node sequenced on");
    running.abort();
    node.stop().await;
    println!("PASS: the bucket holds every finalized range through an outage, two copies and a restart");
}

/// Checks that the bucket holds each of the first `whole` ranges of the log in `dir`, compressed.
fn check_objects(bucket: &StandIn, dir: &Path, view: &Manifest, whole: u64) {
    let objects = bucket.with(|b| b.objects.clone());
    for index in 0..whole {
        let key = range_key(&test_log(), index);
        let raw = range_bytes(dir, view, index).unwrap();
        assert!(objects.get(&key).is_some_and(|object| holds(object, &raw)), "{key}");
    }
}

/// Submits ticks until `node` has made at least `ranges` ranges whole.
pub(super) async fn ticks_until(node: &TestSequencer, ranges: u64) {
    while whole_ranges(&node.manifest.borrow()).unwrap() < ranges {
        node.submit(Command::Tick).await;
    }
}

/// Has `vault` finalize a checkpoint past every range `node` has made whole, ticking until one is, waits until
/// `metrics` confirm the ranges below it, and returns how many.
pub(super) async fn finalize_all(node: &TestSequencer, dir: &Path, vault: &Vault, metrics: &Metrics) -> u64 {
    let whole = finalize_all_after(node, dir, vault, 0).await;
    until("the finalized ranges confirmed", || metrics.archive.confirmed_ranges.get() as u64 >= whole).await;
    whole
}

/// Ticks `node` until a checkpoint it wrote to a completed segment of `dir` lies past every range it has made
/// whole, and at least `ranges` of them, has `vault` finalize it, and returns how many ranges are below it.
async fn finalize_all_after(node: &TestSequencer, dir: &Path, vault: &Vault, ranges: u64) -> u64 {
    loop {
        let completed = node.manifest.borrow().unlisted_from().unwrap();
        let whole = completed / RANGE_SEQS;
        let last = checkpoints(dir).into_iter().rfind(|(seq, _)| *seq < completed);
        if let Some((seq, root)) = last.filter(|(seq, _)| whole >= ranges && *seq >= whole * RANGE_SEQS) {
            vault.finalize(seq, &root);
            return whole;
        }
        node.submit(Command::Tick).await;
    }
}

/// Writes a copy of the log in `dir` to `copy` in segments of five commands, sealed early once past its middle as
/// a handover seals one, after ticking `node` until the copy's completed segments hold the checkpoint past range
/// `ranges`.
async fn copy_of(node: &TestSequencer, dir: &Path, copy: &Path, ranges: u64) {
    ticks_until(node, ranges + 2).await;
    let entries = log_entries(dir);
    let seal_after = (entries.len() / 2..entries.len() - 1)
        .find(|n| matches!(entries[n + 1], LogEntry::Command(_)))
        .expect("a command past the middle");
    resegmented(&entries, copy, 5, Some(seal_after));
}

/// An uploader of `dir` to `bucket` below `vault`'s finality, warning after `lag`, its sources and its metrics.
pub(super) fn uploading(
    bucket: &StandIn,
    dir: &Path,
    lag: Duration,
    vault: &Vault,
) -> (Uploader<Vault>, crate::archive::Sources, Arc<Metrics>) {
    let metrics = Arc::new(Metrics::new());
    let (uploader, sources) =
        Uploader::new(bucket.store(&test_log()), dir.to_owned(), lag, metrics.clone(), Some(vault.clone()));
    (uploader, sources, metrics)
}

/// Waits until `done` holds.
pub(super) async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < WAIT, "timed out waiting for {what}");
        sleep(Duration::from_millis(10)).await;
    }
}

/// Drops what the alerts and resolutions collected so far hold, once a moment has passed for the last to arrive.
async fn quiet(
    alerts: &mut tokio::sync::mpsc::UnboundedReceiver<Alert>,
    resolved: &mut tokio::sync::mpsc::UnboundedReceiver<Alert>,
) {
    sleep(Duration::from_millis(100)).await;
    while alerts.try_recv().is_ok() || resolved.try_recv().is_ok() {}
}

/// The next alert, or resolution, named `name`.
async fn expect(alerts: &mut tokio::sync::mpsc::UnboundedReceiver<Alert>, name: &str) -> BTreeMap<String, String> {
    timeout(WAIT, async {
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
