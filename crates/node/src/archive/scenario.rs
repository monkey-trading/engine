//! The log archive's dev scenario (`scripts/dev-log-archive.sh`): a dev node uploads its log's ranges to a loopback
//! stand-in for the bucket as the vault finalizes them, through an outage, a member's differently cut copy and a
//! restart, until a read-back differs.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engine_core::command::Command;
use tokio::time::{Instant, sleep, timeout};

use crate::archive::range::tests::resegmented;
use crate::archive::stand_in::{StandIn, Vault, test_log};
use crate::archive::{RANGE_SEQS, Uploader, holds, range_bytes, range_key, whole_ranges};
use crate::log::{LogEntry, Manifest};
use crate::metrics::Metrics;
use crate::test_support::{
    Alert, TestSequencer, capture_alerts_and_resolutions, log_entries, published_manifest, scratch_dir,
};

/// How long the scenario waits for any one step.
const WAIT: Duration = Duration::from_secs(60);

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
    *vault.0.lock().unwrap() = Some(3 * RANGE_SEQS);
    until("three ranges confirmed", || metrics.archive.confirmed_ranges.get() == 3).await;
    *vault.0.lock().unwrap() = Some(u64::MAX - 1);
    let whole = confirmed_through(&node, &metrics).await;
    check_objects(&bucket, &dir, &node.manifest.borrow().clone(), whole);
    let (raw, stored) = (metrics.archive.confirmed_bytes.get(), metrics.archive.stored_bytes.get());
    println!("    3 ranges below the checkpoint first, then {whole}; {raw} bytes stored as {stored}");

    println!("==> 2. the bucket goes down: sequencing goes on, the backlog grows and archive_upload_lagging fires");
    bucket.with(|b| b.down = true);
    ticks_until(&node, whole + 4).await;
    expect(&mut alerts, "archive_upload_lagging").await;
    until("the outage's ranges counted", || metrics.archive.pending_ranges.get() >= 4).await;
    let pending = metrics.archive.pending_ranges.get();
    bucket.with(|b| b.down = false);
    expect(&mut resolved, "archive_upload_lagging").await;
    let whole = confirmed_through(&node, &metrics).await;
    println!("    {pending} ranges waited through the outage; caught up through {whole}");

    println!("==> 3. a member's copy, cut into other segments, uploads the same ranges and changes no object");
    let copy = scratch_dir("archive-scenario-copy");
    let entries = log_entries(&dir);
    let seal_after = (entries.len() / 2..entries.len() - 1)
        .find(|n| matches!(entries[n + 1], LogEntry::Command(_)))
        .expect("a command past the middle");
    resegmented(&entries, &copy, 5, Some(seal_after));
    let held = bucket.with(|b| b.objects.clone());
    let (member, member_sources, member_metrics) = uploading(&bucket, &copy, lag, &vault);
    member_sources.send_replace(Some(published_manifest(&copy)));
    let member_running = tokio::spawn(member.run());
    let copied = whole_ranges(&published_manifest(&copy).borrow()).unwrap();
    let found = || member_metrics.archive.confirmed_ranges.get() as u64 >= copied;
    until("the member's copy found in the bucket", found).await;
    member_running.abort();
    assert_eq!(bucket.with(|b| b.objects.clone()), held, "the bucket is as it was");
    assert_eq!(member_metrics.archive.objects_created.get(), 0);
    println!("    the member's {copied} ranges are all in the bucket already, byte for byte");

    println!("==> 4. the node restarts, and its uploader resumes at the first missing range");
    running.abort();
    node.stop().await;
    let node = TestSequencer::start(&dir, 256);
    ticks_until(&node, whole + 2).await;
    let (asked, read) = bucket.with(|b| (b.asked, b.read));
    let (uploader, sources, metrics) = uploading(&bucket, &dir, lag, &vault);
    sources.send_replace(Some(node.manifest.clone()));
    running = tokio::spawn(uploader.run());
    let now = confirmed_through(&node, &metrics).await;
    let (asked, read) = bucket.with(|b| (b.asked - asked, b.read - read));
    assert!(asked <= u64::from(u64::BITS - now.leading_zeros()) + 1, "{asked} existence checks for {now}");
    assert_eq!(read, 1 + now - whole, "the range before the new ones, and each new one, read back");
    check_objects(&bucket, &dir, &node.manifest.borrow().clone(), now);
    println!("    resumed with {asked} existence checks and {read} reads, and confirmed through {now}");

    println!("==> 5. the bucket serves other bytes for the next range: the Warning fires, and the uploads stop there");
    let key = range_key(&test_log(), now);
    bucket.with(|b| b.served.insert(key.clone(), b"other bytes".to_vec()));
    ticks_until(&node, now + 1).await;
    let alert = expect(&mut alerts, "archive_object_mismatch").await;
    assert_eq!(alert["object"], key);
    let created = bucket.with(|b| b.created);
    ticks_until(&node, now + 4).await;
    until("the ranges after it counted", || metrics.archive.pending_ranges.get() >= 4).await;
    assert_eq!(bucket.with(|b| b.created), created, "no range goes up past the mismatch");
    assert_eq!(metrics.archive.confirmed_ranges.get() as u64, now);
    println!("    {key} reported, and nothing past it went up while the node sequenced on");
    running.abort();
    node.stop().await;
    println!("PASS: the bucket holds every finalized range through an outage, a member's copy and a restart");
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
async fn ticks_until(node: &TestSequencer, ranges: u64) {
    while whole_ranges(&node.manifest.borrow()).unwrap() < ranges {
        node.submit(Command::Tick).await;
    }
}

/// Waits until `metrics` confirm every range `node` has made whole, and returns how many that is.
async fn confirmed_through(node: &TestSequencer, metrics: &Metrics) -> u64 {
    let whole = whole_ranges(&node.manifest.borrow()).unwrap();
    until("the whole ranges confirmed", || metrics.archive.confirmed_ranges.get() as u64 >= whole).await;
    whole
}

/// An uploader of `dir` to `bucket` below `vault`'s finality, warning after `lag`, its sources and its metrics.
fn uploading(
    bucket: &StandIn,
    dir: &Path,
    lag: Duration,
    vault: &Vault,
) -> (Uploader<Vault>, crate::archive::Sources, Arc<Metrics>) {
    let metrics = Arc::new(Metrics::new());
    let (uploader, sources) =
        Uploader::new((bucket.store(), test_log()), dir.to_owned(), lag, metrics.clone(), Some(vault.clone()));
    (uploader, sources, metrics)
}

/// Waits until `done` holds.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < WAIT, "timed out waiting for {what}");
        sleep(Duration::from_millis(10)).await;
    }
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
