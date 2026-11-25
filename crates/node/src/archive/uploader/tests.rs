use super::*;

use std::path::Path;

use engine_core::command::Command;

use crate::archive::stand_in::{StandIn, Vault, test_log};
use crate::test_support::{Alert, TestSequencer, capture_alerts, capture_alerts_and_resolutions, scratch_dir};

/// How long a test waits for the uploader before it fails.
const WAIT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn every_whole_range_is_confirmed_in_its_object() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-every-range"));
    let node = node_with(&dir, 4).await;
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let whole = confirmed_through(&node, &metrics).await;
    let view = node.manifest.borrow().clone();
    for index in 0..whole {
        let object = bucket.with(|b| b.objects[&range_key(&test_log(), index)].clone());
        assert!(holds(&object, &range_bytes(&dir, &view, index).unwrap()), "range {index} in its object");
    }
    assert_eq!(bucket.with(|b| b.created), whole, "one object a range");
    assert_eq!(metrics.archive.mismatches.get(), 0);
    running.abort();
    node.stop().await;
}

/// A range goes up only once its last seq is below the vault's finalized checkpoint: none before the vault's
/// first, the ranges below it once it is read, and the rest as it moves on.
#[tokio::test]
async fn a_range_waits_for_the_vaults_finalized_checkpoint() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-finality"));
    let node = node_with(&dir, 4).await;
    let vault = Vault::default();
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), Some(vault.clone()));
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    until("the whole ranges counted", || metrics.archive.pending_ranges.get() >= 4).await;
    tokio::time::sleep(10 * FINALITY_EVERY).await;
    assert_eq!(bucket.with(|b| b.created), 0, "nothing goes up before the vault's first finalized checkpoint");
    // A checkpoint at the second range's last seq: the first range ends below it, the second does not.
    *vault.0.lock().unwrap() = Some(2 * RANGE_SEQS - 1);
    until("the first range confirmed", || metrics.archive.confirmed_ranges.get() == 1).await;
    tokio::time::sleep(10 * FINALITY_EVERY).await;
    assert_eq!(bucket.with(|b| b.objects.keys().cloned().collect::<Vec<_>>()), [range_key(&test_log(), 0)]);
    *vault.0.lock().unwrap() = Some(u64::MAX - 1);
    confirmed_through(&node, &metrics).await;
    running.abort();
    node.stop().await;
}

#[tokio::test]
async fn an_outage_holds_ranges_back_and_raises_then_clears_the_lag_alert() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-outage"));
    let node = node_with(&dir, 2).await;
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_millis(300), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let before = confirmed_through(&node, &metrics).await;

    bucket.with(|b| b.down = true);
    let created = bucket.with(|b| b.created);
    // Sequencing goes on through the outage.
    more(&node, before + 3).await;
    recv(&mut alerts, "archive_upload_lagging").await;
    until("the outage's ranges counted", || metrics.archive.pending_ranges.get() >= 3).await;
    assert!(metrics.archive.failures.get() > 0);
    assert_eq!(bucket.with(|b| b.created), created, "nothing is created while the bucket is down");

    bucket.with(|b| b.down = false);
    recv(&mut resolved, "archive_upload_lagging").await;
    confirmed_through(&node, &metrics).await;
    assert_eq!(metrics.archive.pending_ranges.get(), 0);
    running.abort();
    node.stop().await;
}

/// A resume that cannot reach the bucket raises the lag alert once it has waited the bound, as a waiting range
/// would; a resume that then succeeds while uploads are refused resolves nothing, since the ranges have waited
/// since the resume began; and the episode ends once the uploads catch up.
#[tokio::test]
async fn a_resume_that_cannot_reach_the_bucket_raises_the_lag_alert() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-resume-down"));
    let node = node_with(&dir, 2).await;
    bucket.with(|b| b.down = true);
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_millis(300), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let alert = recv(&mut alerts, "archive_upload_lagging").await;
    assert!(alert["message"].contains("to resume"), "{alert:?}");
    bucket.with(|b| (b.down, b.refuse_uploads) = (false, true));
    until("the resume's end", || metrics.archive.pending_ranges.get() > 0).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!drained(&mut resolved, "archive_upload_lagging"), "the ranges have waited since the resume began");
    bucket.with(|b| b.refuse_uploads = false);
    recv(&mut resolved, "archive_upload_lagging").await;
    confirmed_through(&node, &metrics).await;
    running.abort();
    node.stop().await;
}

/// A range made whole after a resume that found the bucket caught up waits from when it was made whole, not from
/// the resume's start.
#[tokio::test]
async fn a_range_made_whole_after_a_caught_up_resume_waits_from_then() {
    let (_guard, mut alerts) = capture_alerts();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-caught-up"));
    let node = node_with(&dir, 2).await;
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_millis(300), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let whole = confirmed_through(&node, &metrics).await;
    running.abort();
    // A restart's resume finds every whole range in the bucket.
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_millis(300), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    confirmed_through(&node, &metrics).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    more(&node, whole + 1).await;
    confirmed_through(&node, &metrics).await;
    assert!(!drained(&mut alerts, "archive_upload_lagging"), "no range waited the bound");
    running.abort();
    node.stop().await;
}

/// A new source carries a lag episode on: a resume that succeeds, while uploads are still refused, resolves
/// nothing, and the episode ends only once the waiting ranges are confirmed.
#[tokio::test]
async fn a_new_source_carries_a_lag_episode_on() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-new-source"));
    let node = node_with(&dir, 2).await;
    bucket.with(|b| b.refuse_uploads = true);
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_millis(300), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    recv(&mut alerts, "archive_upload_lagging").await;
    let asked = bucket.with(|b| b.asked);
    sources.send_replace(Some(node.manifest.clone()));
    until("the new source's resume", || bucket.with(|b| b.asked) > asked).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!drained(&mut resolved, "archive_upload_lagging"), "a resume confirms nothing");
    bucket.with(|b| b.refuse_uploads = false);
    recv(&mut resolved, "archive_upload_lagging").await;
    confirmed_through(&node, &metrics).await;
    running.abort();
    node.stop().await;
}

#[tokio::test]
async fn an_object_that_differs_is_reported_once_and_the_uploads_stop_there_across_a_restart() {
    let (_guard, mut alerts) = capture_alerts();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-mismatch"));
    let node = node_with(&dir, 3).await;
    let key = range_key(&test_log(), 1);
    // The bucket's public origin serves other bytes for the second range than it holds.
    bucket.with(|b| b.served.insert(key.clone(), compress(b"other bytes\n").unwrap()));
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let alert = recv(&mut alerts, "archive_object_mismatch").await;
    assert_eq!(alert["object"], key);
    assert_eq!(alert["first_seq"], RANGE_SEQS.to_string());
    // The writer goes on, and no range past the mismatch goes up.
    let whole = whole_ranges(&node.manifest.borrow()).unwrap();
    more(&node, whole + 2).await;
    until("the backlog counted", || metrics.archive.pending_ranges.get() as u64 > whole).await;
    assert_eq!(metrics.archive.confirmed_ranges.get(), 1, "the range before it only");
    assert_eq!(bucket.with(|b| b.created), 2, "the first range and the one that reads back otherwise");
    assert!(!drained(&mut alerts, "archive_object_mismatch"), "reported once");
    running.abort();

    // A restart's resume reads back the range before the first it lacks, and stops there too.
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let alert = recv(&mut alerts, "archive_object_mismatch").await;
    assert_eq!(alert["object"], key);
    until("the backlog counted", || metrics.archive.pending_ranges.get() as u64 > whole).await;
    assert_eq!((metrics.archive.confirmed_ranges.get(), bucket.with(|b| b.created)), (1, 2));
    running.abort();
    node.stop().await;
}

/// A range found otherwise stays the end across a change of writer, though another member has since uploaded the
/// ranges past it: the new source's resume finds them, and the node still uploads nothing past the mismatch.
#[tokio::test]
async fn a_mismatch_stays_the_end_across_a_change_of_writer() {
    let (_guard, mut alerts) = capture_alerts();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-mismatch-source"));
    let node = node_with(&dir, 3).await;
    bucket.with(|b| b.served.insert(range_key(&test_log(), 1), compress(b"other bytes\n").unwrap()));
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    recv(&mut alerts, "archive_object_mismatch").await;
    // Another member uploads the ranges past it, as its own copy holds them.
    more(&node, 5).await;
    let view = node.manifest.borrow().clone();
    for index in 2..5 {
        let object = compress(&range_bytes(&dir, &view, index).unwrap()).unwrap();
        bucket.with(|b| b.objects.insert(range_key(&test_log(), index), object));
    }
    let (created, asked) = bucket.with(|b| (b.created, b.asked));
    sources.send_replace(Some(node.manifest.clone()));
    until("the new source's resume", || bucket.with(|b| b.asked) > asked).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!((metrics.archive.confirmed_ranges.get(), bucket.with(|b| b.created)), (1, created));
    running.abort();
    node.stop().await;
}

/// A node stopped at a mismatch while its writer is quiet still raises the lag alert once the range has waited.
#[tokio::test]
async fn a_quiet_writer_still_raises_the_lag_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-quiet"));
    let node = node_with(&dir, 3).await;
    bucket.with(|b| b.served.insert(range_key(&test_log(), 1), compress(b"other bytes\n").unwrap()));
    let (uploader, sources, _metrics) = uploading(&bucket, &dir, Duration::from_millis(300), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    recv(&mut alerts, "archive_object_mismatch").await;
    // No command is sequenced from here on, so the writer's manifest does not change.
    let alert = recv(&mut alerts, "archive_upload_lagging").await;
    assert!(alert["message"].contains("whole ranges"), "{alert:?}");
    running.abort();
    node.stop().await;
}

/// A restart finds the first missing range with existence checks, and reads back only the range before it and
/// the ranges it then uploads.
#[tokio::test]
async fn a_restart_resumes_at_the_first_missing_range_reading_back_only_the_one_before() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-resume"));
    let node = node_with(&dir, 16).await;
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let whole = confirmed_through(&node, &metrics).await;
    running.abort();
    let (created, asked, read) = bucket.with(|b| (b.created, b.asked, b.read));

    // Two ranges more, made whole while no uploader ran.
    more(&node, whole + 2).await;
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let now = confirmed_through(&node, &metrics).await;
    let (created, asked, read) = bucket.with(|b| (b.created - created, b.asked - asked, b.read - read));
    assert_eq!(created, now - whole, "only the new ranges are created");
    assert_eq!(read, 1 + now - whole, "the range before them and each new one are read back");
    let bound = u64::from(u64::BITS - now.leading_zeros()) + 1;
    assert!(asked <= bound, "{asked} existence checks for {now} ranges");
    running.abort();
    node.stop().await;
}

#[tokio::test]
async fn a_range_already_in_the_bucket_is_confirmed_and_left_as_it_was() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-existing"));
    let node = node_with(&dir, 2).await;
    let view = node.manifest.borrow().clone();
    // Another member's upload of the same range, compressed otherwise.
    let theirs = zstd::bulk::compress(&range_bytes(&dir, &view, 0).unwrap(), 19).unwrap();
    bucket.with(|b| b.objects.insert(range_key(&test_log(), 0), theirs.clone()));
    let (uploader, _sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), None);
    let mut work = uploader.work;
    assert_eq!(work.range(&view, 0).await.unwrap(), Found::Confirmed);
    assert_eq!(bucket.with(|b| (b.refused, b.objects[&range_key(&test_log(), 0)].clone())), (1, theirs));
    assert_eq!((metrics.archive.mismatches.get(), metrics.archive.objects_created.get()), (0, 0));
    node.stop().await;
}

#[tokio::test]
async fn a_range_whose_segment_differs_from_its_listing_is_never_uploaded() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-altered-file"));
    let node = node_with(&dir, 2).await;
    let listed = node.manifest.borrow().completed_at(0).unwrap();
    let mut raw = std::fs::read(dir.join(&listed.file)).unwrap();
    raw[1] ^= 1;
    std::fs::write(dir.join(&listed.file), raw).unwrap();
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    until("a failed upload", || metrics.archive.failures.get() >= 2).await;
    assert_eq!(bucket.with(|b| b.created), 0, "nothing of the log after it goes up either");
    assert_eq!(metrics.archive.confirmed_ranges.get(), 0);
    running.abort();
    node.stop().await;
}

/// The node's uploader comes from `[archive]` and both of its delivered keys, and with either missing there is
/// none; without `[checkpoints]` it reads no vault.
#[test]
fn the_uploader_runs_only_with_the_section_and_both_keys() {
    let mut cfg = crate::test_support::dev_config();
    let metrics = Arc::new(Metrics::new());
    assert!(Uploader::for_config(&cfg, &Values::new(), &metrics).unwrap().is_none(), "no section");
    let section: crate::config::ArchiveSection = toml::from_str(
        "endpoint = 'https://account.r2.cloudflarestorage.com'\nbucket = 'engine-log'\nregion = 'auto'\n\
         public_url = 'https://archive.example.com'\naccess_key_id_secret = 'archive_access_key_id'\n\
         secret_access_key_secret = 'archive_secret_access_key'\n",
    )
    .unwrap();
    let names = (section.access_key_id_secret.clone(), section.secret_access_key_secret.clone());
    cfg.archive = Some(section);
    let value = crate::config::SecretValue::test;
    let one: Values = [(names.0.clone(), value("FAKE-id"))].into();
    let both: Values = [(names.0, value("FAKE-id")), (names.1, value("FAKE-secret"))].into();
    for (keys, runs) in [(Values::new(), false), (one, false), (both, true)] {
        let uploader = Uploader::for_config(&cfg, &keys, &metrics).unwrap();
        assert_eq!(uploader.is_some(), runs, "{} keys", keys.len());
        assert!(uploader.is_none_or(|(uploader, _)| uploader.work.finality.is_none()), "no vault without checkpoints");
    }
}

// ── Helpers ──

/// A dev node on `dir` with at least `ranges` whole ranges.
async fn node_with(dir: &Path, ranges: u64) -> TestSequencer {
    let node = TestSequencer::start(dir, 256);
    more(&node, ranges).await;
    node
}

/// Submits ticks until `node` has made at least `ranges` ranges whole.
async fn more(node: &TestSequencer, ranges: u64) {
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

/// An uploader of `dir` to `bucket` below `vault`'s finality, if any, warning after `lag`, its sources and its
/// metrics.
fn uploading(
    bucket: &StandIn,
    dir: &Path,
    lag: Duration,
    vault: Option<Vault>,
) -> (Uploader<Vault>, Sources, Arc<Metrics>) {
    let metrics = Arc::new(Metrics::new());
    let (uploader, sources) = Uploader::new((bucket.store(), test_log()), dir.to_owned(), lag, metrics.clone(), vault);
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

/// Whether any alert, or resolution, collected so far is named `name`.
fn drained(alerts: &mut tokio::sync::mpsc::UnboundedReceiver<Alert>, name: &str) -> bool {
    std::iter::from_fn(|| alerts.try_recv().ok())
        .filter(|alert| alert.get("alert").or_else(|| alert.get("alert_resolved")).is_some_and(|raised| raised == name))
        .count()
        > 0
}

/// The next alert, or resolution, named `name`.
async fn recv(alerts: &mut tokio::sync::mpsc::UnboundedReceiver<Alert>, name: &str) -> Alert {
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
