//! The daily snapshots' archive, which `scripts/dev-snapshot-archive.sh` runs, on deterministic time with isolated
//! temporary state and a loopback stand-in for the bucket: a node writing launch flow on a small book across four
//! UTC midnights publishes a daily snapshot at its first checkpoint and at its first past each midnight, and its
//! uploader puts each into the bucket as it is published and reads it back; the snapshots stamped more than the
//! window (36 hours) before the newest leave the host, but for the newest of them, while the manifest lists them all
//! and the node's archive answers `308` to the bucket's object for each; `engine replay --snapshots <the node's URL>`
//! downloads every listed snapshot, those gone from the host from the bucket, and replays the log in ranges to what
//! the whole log replays to; a member's snapshot start takes one the node holds no longer from the bucket; and a
//! bucket object whose bytes are altered is refused by both.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use engine_core::types::AccountId;

use crate::archive::origin::copy_log;
use crate::archive::stand_in::StandIn;
use crate::clock::Clock;
use crate::common::launch_book::{NamedCounts, build};
use crate::common::{CHAIN_ID, VAULT};
use crate::daily_snapshots::{
    self, Listed, PublishedSnapshots, SnapshotBucket, SnapshotUploader, serve_snapshots, snapshot_start,
    snapshots_prefix,
};
use crate::log::SealChecks;
use crate::metrics::Metrics;
use crate::sequencer::recovery_scale::flow::{Flow, Node};
use crate::sequencer::recovery_scale::orders::Traders;
use crate::sequencer::recovery_scale::restart::DOMAIN;
use crate::sequencer::recovery_scale::{Base, mix};
use crate::sequencer::replay::ranges::{Trust, replay_ranges_from};
use crate::sequencer::replay::{Audited, replay_path_from};
use crate::standby::archive::archive_client;
use crate::tee::EnclaveKey;
use crate::test_support::{capture_alerts_everywhere, scratch_dir};

/// A UTC day, and how long before each midnight the flow starts and how long it runs.
const DAY_MS: i64 = 86_400_000;
const BEFORE_MIDNIGHT: Duration = Duration::from_secs(60);
const FLOW: Duration = Duration::from_secs(120);
/// How long the host keeps a snapshot the bucket holds: a day and a half, so the window's edge falls between days.
const WINDOW: Duration = Duration::from_secs(36 * 3_600);
/// How long the scenario waits for the uploader at any one step.
const WAIT: Duration = Duration::from_secs(60);

#[test]
#[ignore = "a dev scenario: scripts/dev-snapshot-archive.sh runs it"]
fn dev_scenario_snapshot_archive() {
    let mut alerts = capture_alerts_everywhere();
    let counts = NamedCounts { adders: 1, openers: 8, closeouts: 0 };
    let mut book = build(&mix(200, 0, 1_200), counts);
    let engine = std::mem::take(&mut book.h.engine);
    let base = Base::new(&engine).expect("the book encodes");
    let traders = Traders::new(std::mem::take(&mut book.named.openers), &engine);
    drop((book, engine));
    let run = scratch_dir("snapshot-archive");
    let (log, snapshots) = (run.join("log"), run.join("snapshots"));
    let key = Arc::new(EnclaveKey::generate());
    let signer = AccountId(key.address());
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    let bucket = runtime.block_on(StandIn::start());
    let prefix = snapshots_prefix(CHAIN_ID, VAULT);
    // The node reads back over TLS to the bucket's origin; a reader that trusts only the public roots reads it over
    // plain http, the same server.
    let moved = SnapshotBucket::new(&bucket.plain, &prefix).expect("the bucket's snapshots");
    let metrics = Arc::new(Metrics::new());
    let uploader = SnapshotUploader::new(
        (bucket.store(), prefix.clone()),
        snapshots.clone(),
        (WINDOW, Duration::from_secs(3_600)),
        metrics.clone(),
    );
    let uploading = runtime.spawn(uploader.run());

    println!("==> 1. a node publishes a daily snapshot each simulated day, and each goes up and reads back");
    let mut node = Node::publishing(base.engine(), &log, key.clone(), Some(&snapshots));
    let mut flow = Flow::new(traders, BTreeMap::from([(1, 11.0)]));
    let confirmed = |count: u64| {
        let started = std::time::Instant::now();
        while metrics.snapshot_archive.confirmed.get() < count {
            assert!(started.elapsed() < WAIT, "the uploader confirmed {count} snapshots in time");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    confirmed(1);
    for day in 1..=4u64 {
        let to_midnight = DAY_MS - node.clock.now_ms().rem_euclid(DAY_MS);
        flow.skip(&mut node, Duration::from_millis(u64::try_from(to_midnight).unwrap()) - BEFORE_MIDNIGHT);
        flow.run(&mut node, FLOW, 1.0, |_, _, _| {});
        node.checkpoint_now();
        confirmed(day + 1);
    }
    let listed = daily_snapshots::read_manifest(&snapshots).expect("the manifest");
    assert_eq!(listed.len(), 5, "a snapshot on each day: {listed:?}");
    for listing in &listed {
        let object = bucket.with(|b| b.objects.get(&format!("{prefix}{}", listing.file)).cloned());
        let held = std::fs::read(snapshots.join(&listing.file)).ok();
        assert!(object.is_some(), "{} is in the bucket", listing.file);
        assert!(held.is_none() || held == object, "{} is in the bucket as written", listing.file);
    }
    println!("    {} snapshots, {} bytes in all, each confirmed", listed.len(), listed.iter().map(|l| l.bytes).sum::<u64>());

    println!("==> 2. those older than the window leave the host, but the newest of them, and are served by 308");
    let held = |listing: &Listed| snapshots.join(&listing.file).exists();
    let started = std::time::Instant::now();
    while metrics.snapshot_archive.removed.get() < 2 {
        assert!(started.elapsed() < WAIT, "the two oldest leave the host in time");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(listed.iter().map(held).collect::<Vec<_>>(), [false, false, true, true, true]);
    assert_eq!(daily_snapshots::read_manifest(&snapshots).unwrap(), listed, "the manifest lists them all");
    let url = runtime.block_on(serve_snapshots(&snapshots, Some(moved.clone())));
    let answer = runtime
        .block_on(bucket.client.get(format!("{url}/v1/archive/snapshots/{}", listed[0].file)).send())
        .expect("the node answers");
    assert_eq!(answer.status(), 308);
    assert_eq!(answer.headers()["location"], moved.object(&listed[0].file).unwrap().as_str());
    println!("    {} and {} answer 308 to the bucket", listed[0].file, listed[1].file);

    println!("==> 3. engine replay --snapshots <the node's URL> replays the log in ranges to the whole log's state");
    drop(node);
    let snapshots_at: PublishedSnapshots = url.parse().expect("a node's URL");
    let local = snapshots_at.local(&log).expect("the snapshots download");
    let whole = Audited::from(replay_path_from(base.engine(), &log, signer, SealChecks::RunEnds).expect("whole"));
    let trust = || Trust::Pinned { domain: None, signer: Some(signer) };
    let jobs = NonZeroUsize::new(2).unwrap();
    let in_ranges =
        replay_ranges_from(&|| base.engine(), &log, local.path(), trust(), jobs).expect("the log replays in ranges");
    assert_eq!(in_ranges.ranges, listed.len() + 1, "a range from each snapshot");
    assert_eq!((whole.commands, whole.checkpoints), (in_ranges.commands, in_ranges.checkpoints));
    let state = |audited: &Audited| serde_json::to_vec(&audited.engine).expect("the engine encodes");
    assert_eq!(state(&whole), state(&in_ranges), "the ranges end in the whole log's state");
    println!("    {} ranges, {} commands, as the whole log", in_ranges.ranges, in_ranges.commands);
    drop(local);

    println!("==> 4. a member's snapshot start takes one the node no longer holds from the bucket");
    let copy = run.join("copy");
    copy_log(&log, &copy);
    let (client, origins, signers) = (archive_client(false).unwrap(), [url.clone()], [signer].into());
    let member = run.join("member-snapshots");
    let bounds = (u64::MAX, Some(listed[2].ts));
    let signed = runtime
        .block_on(snapshot_start(&client, &origins, (&member, &copy), DOMAIN, &signers, bounds))
        .expect("the member starts from a snapshot");
    assert_eq!(signed.snapshot.offset.seq, listed[1].seq, "the newest before the bound, from the bucket");
    let object = bucket.with(|b| b.objects[&format!("{prefix}{}", listed[1].file)].clone());
    assert_eq!(std::fs::read(member.join(&listed[1].file)).unwrap(), object);
    println!("    the member's replica starts from {}", listed[1].file);

    println!("==> 5. a bucket object whose bytes are altered is refused by the replay and the member's start");
    let altered_key = format!("{prefix}{}", listed[0].file);
    bucket.with(|b| {
        let mut altered = b.objects[&altered_key].clone();
        let at = altered.len() / 2;
        altered[at] ^= 1;
        b.served.insert(altered_key.clone(), altered)
    });
    let error = format!("{:#}", snapshots_at.local(&log).expect_err("an altered snapshot is refused"));
    assert!(error.contains(&listed[0].file) && error.contains("differs from its listing"), "{error}");
    while alerts.try_recv().is_ok() {}
    let member = run.join("member-snapshots-altered");
    let bounds = (u64::MAX, Some(listed[1].ts));
    let refused = runtime.block_on(snapshot_start(&client, &origins, (&member, &copy), DOMAIN, &signers, bounds));
    assert!(refused.is_none(), "the altered snapshot is not taken");
    let alert = std::iter::from_fn(|| alerts.try_recv().ok())
        .find(|alert| alert.get("alert").is_some_and(|name| name == "snapshot_start_refused"))
        .expect("the refusal is paged");
    assert!(alert["message"].contains("differs from its listing"), "{alert:?}");
    println!("    refused: {error}");
    uploading.abort();
    drop(bucket);
    let _ = std::fs::remove_dir_all(&run);
    println!("PASS: every daily snapshot is in the bucket, and the host keeps the window's");
}
