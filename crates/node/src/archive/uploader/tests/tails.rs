//! The tails of the checkpoints the vault may still hold: up before the vault finalizes them, from the first range the
//! bucket lacks, for every checkpoint from the vault's finalized one on, read back, through a failed read of the copy,
//! and none from a copy stalled at a range or superseded.

use super::*;

use std::collections::BTreeSet;

use crate::archive::read::origin::upload;
use crate::archive::read::read_through;
use crate::archive::stand_in::{checkpoints, newest_checkpoint, tail_key};
use crate::archive::{Bucket, piece_bytes, whole_ranges};

/// A root no log of these tests has.
const OTHER_ROOT: &str = "0xabababababababababababababababababababababababababababababababab";

/// The newest checkpoint's tail goes up while the vault has finalized only an earlier one: its pieces run from the
/// first range the bucket lacks, below which no piece goes up, to the checkpoint's own, each holding the copy's.
#[tokio::test]
async fn a_checkpoints_tail_goes_up_before_the_vault_finalizes_it() {
    let (bucket, dir, vault) = (StandIn::start().await, scratch_dir("archive-tail-early"), Vault::default());
    let node = node_with(&dir, 2).await;
    let finalized = vault.finalize_past(&node, &dir, 1).await;
    more(&node, 6).await;
    let (seq, root) = newest_checkpoint(&dir);
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), Some(vault));
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    until("the newest checkpoint's tail", || metrics.archive.tail_seq.get() as u64 == seq).await;
    let ranges = metrics.archive.confirmed_ranges.get() as u64;
    assert_eq!(ranges, finalized / RANGE_SEQS, "no range past the vault's finalized checkpoint");
    assert!(seq / RANGE_SEQS > ranges, "the tail covers ranges the bucket lacks");
    let view = node.manifest.borrow().clone();
    for index in ranges..=seq / RANGE_SEQS {
        let object = bucket.with(|b| b.objects.get(&tail_key(seq, &root, index)).cloned()).expect("the piece");
        assert!(holds(&object, &piece_bytes(&dir, &view, index, seq).unwrap()), "piece {index}");
    }
    assert!(bucket.with(|b| !b.objects.contains_key(&tail_key(seq, &root, ranges - 1))), "none below them");
    running.abort();
    node.stop().await;
}

/// Every checkpoint the copy holds from the vault's finalized one on gets a tail, since the vault's latest may be any of
/// them, a handover's, a shutdown's or an activation's checkpoint coming between paced ones, and the log reads through
/// each from the bucket alone. Once the vault finalizes a later checkpoint, the older ones still waiting get none.
#[tokio::test]
async fn every_checkpoint_from_the_vaults_finalized_one_gets_a_tail() {
    let (bucket, dir, vault) = (StandIn::start().await, scratch_dir("archive-tail-every"), Vault::default());
    let node = node_with(&dir, 2).await;
    let finalized = vault.finalize_past(&node, &dir, 1).await;
    bucket.with(|b| b.refuse_uploads = true);
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), Some(vault.clone()));
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    more(&node, 6).await;
    until("a failed upload", || metrics.archive.failures.get() > 0).await;
    bucket.with(|b| b.refuse_uploads = false);
    let (newest, _) = newest_checkpoint(&dir);
    until("the newest tail", || metrics.archive.tail_seq.get() as u64 == newest).await;
    let held: Vec<(u64, String)> = checkpoints(&dir).into_iter().filter(|(seq, _)| *seq >= finalized).collect();
    assert!(held.len() > 2, "{held:?}");
    assert_eq!(tails(&bucket), held.iter().map(|(seq, _)| *seq).collect::<BTreeSet<_>>(), "a tail for each");
    let log = Bucket::new(&bucket.url, &test_log()).unwrap();
    for (seq, root) in &held {
        read_through(&bucket.client, &log, (*seq, root.parse().unwrap()), &mut Vec::new()).await.unwrap();
    }

    // The vault finalizes a later checkpoint while the tails of the ones before it wait.
    let before = tails(&bucket);
    bucket.with(|b| b.refuse_uploads = true);
    let whole = whole_ranges(&node.manifest.borrow()).unwrap();
    more(&node, whole + 3).await;
    let completed = node.manifest.borrow().unlisted_from().unwrap();
    let later = checkpoints(&dir).into_iter().rfind(|(seq, _)| (newest + RANGE_SEQS..completed).contains(seq));
    let (later, later_root) = later.expect("a later checkpoint in a completed segment");
    vault.finalize(later, &later_root);
    let reads = vault.reads();
    // The first read after it takes the later checkpoint as the bound, and the ranges below it wait for the bucket.
    until("the vault's later finality read", || vault.reads() > reads).await;
    bucket.with(|b| b.refuse_uploads = false);
    let (newest_now, _) = newest_checkpoint(&dir);
    until("the newest tail", || metrics.archive.tail_seq.get() as u64 == newest_now).await;
    let added: BTreeSet<u64> = tails(&bucket).difference(&before).copied().collect();
    let written: Vec<u64> = checkpoints(&dir).into_iter().map(|(seq, _)| seq).filter(|seq| *seq > newest).collect();
    assert!(written.iter().any(|seq| *seq < later), "a checkpoint between: {written:?}");
    assert_eq!(
        added,
        written.into_iter().filter(|seq| *seq >= later).collect::<BTreeSet<_>>(),
        "none older than {later}"
    );
    running.abort();
    node.stop().await;
}

/// A read of the copy that fails, here on an open segment the host changed, queues nothing, and raises the lag alert
/// once it has failed for the bound, though no tail was queued yet; once the copy reads again, the next read starts
/// afresh from the first range the bucket lacks and queues each checkpoint it passed once, and the alert resolves.
#[tokio::test]
async fn a_failed_read_of_the_copy_queues_its_checkpoints_once_it_reads_again() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-tail-scan-fails"));
    let node = node_with(&dir, 2).await;
    until("an open segment", || node.manifest.borrow().active.is_some()).await;
    let open = dir.join(&node.manifest.borrow().active.as_ref().unwrap().info.file);
    let raw = std::fs::read(&open).unwrap();
    let mut altered = raw.clone();
    altered[1] ^= 1;
    std::fs::write(&open, &altered).unwrap();
    // Every whole range is in the bucket already, so only the read of the copy waits.
    let view = node.manifest.borrow().clone();
    let whole = whole_ranges(&view).unwrap();
    upload(&bucket, &dir, &view, whole);
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_millis(300), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let alert = recv(&mut alerts, "archive_upload_lagging").await;
    assert!(alert["message"].contains("checkpoints wait to be read"), "{alert:?}");
    assert_eq!(bucket.with(|b| b.tail_created), 0, "nothing goes up from a read that failed");
    std::fs::write(&open, &raw).unwrap();
    let (newest, _) = newest_checkpoint(&dir);
    until("the newest tail", || metrics.archive.tail_seq.get() as u64 == newest).await;
    recv(&mut resolved, "archive_upload_lagging").await;
    let from = whole * RANGE_SEQS;
    let passed: BTreeSet<u64> = checkpoints(&dir).into_iter().map(|(seq, _)| seq).filter(|seq| *seq >= from).collect();
    assert_eq!(tails(&bucket), passed, "a tail for each checkpoint from the first range the bucket lacks");
    let pieces = bucket.with(|b| b.objects.keys().filter(|key| key.contains("/tail/")).count() as u64);
    assert_eq!(bucket.with(|b| b.tail_read), pieces, "each piece read back once: no checkpoint queued twice");
    running.abort();
    node.stop().await;
}

/// A piece of a tail that reads back as other bytes pages `archive_object_mismatch` once, naming it, and that tail goes
/// no further, while the ranges and the later checkpoints' tails go on: here the vault's finality holds the ranges back,
/// so the tail has pieces after the one that reads back otherwise, and nothing else confirms it.
#[tokio::test]
async fn a_tail_piece_that_reads_back_otherwise_pages_once_and_its_tail_stops() {
    let (_guard, mut alerts) = capture_alerts();
    let (bucket, dir, vault) = (StandIn::start().await, scratch_dir("archive-tail-mismatch"), Vault::default());
    let node = node_with(&dir, 2).await;
    let finalized = vault.finalize_past(&node, &dir, 1).await;
    more(&node, 4).await;
    let (seq, root) = newest_checkpoint(&dir);
    let first = finalized / RANGE_SEQS;
    assert!(seq / RANGE_SEQS > first, "the tail has pieces after its first");
    let key = tail_key(seq, &root, first);
    bucket.with(|b| b.served.insert(key.clone(), compress(b"other bytes\n").unwrap()));
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), Some(vault));
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let alert = recv(&mut alerts, "archive_object_mismatch").await;
    assert_eq!(alert["object"], key);
    assert_eq!(alert["first_seq"], (first * RANGE_SEQS).to_string());
    // The later checkpoints' tails go up past it, the ranges held where the vault's finality holds them.
    more(&node, 6).await;
    let (newest, _) = newest_checkpoint(&dir);
    until("a later checkpoint's tail", || metrics.archive.tail_seq.get() as u64 == newest).await;
    assert_eq!(metrics.archive.confirmed_ranges.get() as u64, first, "the ranges wait for the vault");
    let rest = bucket
        .with(|b| (first + 1..=seq / RANGE_SEQS).any(|index| b.objects.contains_key(&tail_key(seq, &root, index))));
    assert!(!rest, "no piece of that tail after the one that reads back otherwise");
    assert!(!drained(&mut alerts, "archive_object_mismatch"), "reported once");
    running.abort();
    node.stop().await;
}

/// A tail the bucket refuses waits as a range would, and raises `archive_upload_lagging` naming it, which resolves
/// once the tail is confirmed.
#[tokio::test]
async fn a_tail_the_bucket_refuses_raises_the_lag_alert() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-tail-lagging"));
    let node = node_with(&dir, 2).await;
    // Every whole range is in the bucket already, so only the tails wait.
    let view = node.manifest.borrow().clone();
    upload(&bucket, &dir, &view, whole_ranges(&view).unwrap());
    bucket.with(|b| b.refuse_uploads = true);
    let (seq, _) = newest_checkpoint(&dir);
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_millis(300), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let alert = recv(&mut alerts, "archive_upload_lagging").await;
    assert!(alert["message"].contains("the tail of the checkpoint at seq"), "{alert:?}");
    assert_eq!(metrics.archive.pending_ranges.get(), 0, "no range waits");
    bucket.with(|b| b.refuse_uploads = false);
    recv(&mut resolved, "archive_upload_lagging").await;
    until("the newest tail", || metrics.archive.tail_seq.get() as u64 == seq).await;
    running.abort();
    node.stop().await;
}

/// A copy stalled at a range that reads back otherwise, or superseded by the vault's history, uploads no tail.
#[tokio::test]
async fn a_stalled_or_superseded_copy_uploads_no_tail() {
    let (_guard, mut alerts) = capture_alerts();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-tail-stalled"));
    let node = node_with(&dir, 3).await;
    bucket.with(|b| b.served.insert(range_key(&test_log(), 1), compress(b"other bytes\n").unwrap()));
    let (uploader, sources, _metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    recv(&mut alerts, "archive_object_mismatch").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(bucket.with(|b| b.tail_created), 0, "stalled at a range");
    running.abort();

    let (bucket, vault) = (StandIn::start().await, Vault::default());
    let (seq, _) = checkpoints(&dir)[1].clone();
    vault.finalize(seq, OTHER_ROOT);
    let (uploader, sources, _metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), Some(vault));
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    recv(&mut alerts, "archive_copy_superseded").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(bucket.with(|b| b.tail_created), 0, "superseded");
    running.abort();
    node.stop().await;
}

/// A tail another copy of the same history uploaded is confirmed as it is, and nothing is created again.
#[tokio::test]
async fn another_copys_tail_is_confirmed_and_left_as_it_was() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-tail-existing"));
    let node = node_with(&dir, 2).await;
    let (seq, _) = newest_checkpoint(&dir);
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    until("the newest tail", || metrics.archive.tail_seq.get() as u64 == seq).await;
    running.abort();
    let (objects, created) = bucket.with(|b| (b.objects.clone(), b.tail_created));
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), None);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    until("the newest tail again", || metrics.archive.tail_seq.get() as u64 == seq).await;
    assert_eq!(bucket.with(|b| (b.objects.clone(), b.tail_created)), (objects, created));
    assert_eq!(metrics.archive.objects_created.get(), 0);
    running.abort();
    node.stop().await;
}

// ── Helpers ──

/// The seqs of the checkpoints whose tails have an object in `bucket`.
fn tails(bucket: &StandIn) -> BTreeSet<u64> {
    bucket.with(|b| {
        b.objects
            .keys()
            .filter_map(|key| key.split_once("/tail/"))
            .filter_map(|(_, tail)| tail.split_once('-'))
            .map(|(seq, _)| seq.parse().unwrap())
            .collect()
    })
}
