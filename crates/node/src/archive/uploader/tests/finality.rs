//! The uploader's bound at the vault's finalized checkpoint, as the copy in use holds it.

use super::*;
use crate::alerts::resolution::ends_episode;
use crate::archive::stand_in::{checkpoint_past, checkpoints};

/// A root no log of the test sequencer's has.
const OTHER_ROOT: &str = "0xabababababababababababababababababababababababababababababababab";

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
    vault.finalize(2 * RANGE_SEQS - 1, &root_at(&dir, 2 * RANGE_SEQS - 1));
    until("the first range confirmed", || metrics.archive.confirmed_ranges.get() == 1).await;
    tokio::time::sleep(10 * FINALITY_EVERY).await;
    assert_eq!(bucket.with(|b| b.objects.keys().cloned().collect::<Vec<_>>()), [range_key(&test_log(), 0)]);
    vault.finalize_past(&node, &dir, 4).await;
    until("the ranges below it confirmed", || metrics.archive.confirmed_ranges.get() >= 4).await;
    running.abort();
    node.stop().await;
}

/// A copy that holds the vault's finalized checkpoint at another root, as a returning operator's superseded tail
/// does, uploads none of its ranges past the last finalized checkpoint it held, and raises
/// `archive_copy_superseded`; a copy handed over afresh that holds it at the vault's root resolves it and uploads on.
#[tokio::test]
async fn a_copy_that_holds_another_history_uploads_nothing_past_the_last_checkpoint_it_held() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-superseded"));
    let node = node_with(&dir, 2).await;
    let vault = Vault::default();
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), Some(vault.clone()));
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    vault.finalize(2 * RANGE_SEQS - 1, &root_at(&dir, 2 * RANGE_SEQS - 1));
    until("the first range confirmed", || metrics.archive.confirmed_ranges.get() == 1).await;

    let (seq, root) = checkpoint_past(&node, &dir, 4).await;
    vault.finalize(seq, OTHER_ROOT);
    let alert = recv(&mut alerts, "archive_copy_superseded").await;
    tokio::time::sleep(10 * FINALITY_EVERY).await;
    assert_eq!(bucket.with(|b| b.created), 1, "nothing past the checkpoint the copy held goes up");
    assert_eq!(metrics.archive.confirmed_ranges.get(), 1);

    vault.finalize(seq, &root);
    sources.send_replace(Some(node.manifest.clone()));
    let resolution = recv(&mut resolved, "archive_copy_superseded").await;
    assert!(ends_episode(&alert, &resolution), "the router ends the raised episode: {alert:?} and {resolution:?}");
    until("the ranges below the checkpoint confirmed", || metrics.archive.confirmed_ranges.get() >= 4).await;
    running.abort();
    node.stop().await;
}

/// A restart whose copy still holds a returning operator's superseded tail reads back no range of it against the
/// bucket's objects, which the successor's history holds otherwise: the copy raises `archive_copy_superseded`, not
/// `archive_object_mismatch`, and uploads on from the copy that replaces it, after reading back the range it held
/// unread.
#[tokio::test]
async fn a_restart_reads_back_no_range_of_a_copy_that_parts_from_the_vaults_history() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-restart-superseded"));
    let node = node_with(&dir, 4).await;
    let view = node.manifest.borrow().clone();
    // The bucket holds the first range, and the successor's bytes for the second, which the copy's tail parts from.
    bucket.with(|b| {
        b.objects.insert(range_key(&test_log(), 0), compress(&range_bytes(&dir, &view, 0).unwrap()).unwrap());
        b.objects.insert(range_key(&test_log(), 1), compress(b"the successor's bytes\n").unwrap());
    });
    let vault = Vault::default();
    vault.finalize(2 * RANGE_SEQS - 1, &root_at(&dir, 2 * RANGE_SEQS - 1));
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), Some(vault.clone()));
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    let (seq, root) = checkpoint_past(&node, &dir, 4).await;
    vault.finalize(seq, OTHER_ROOT);
    recv(&mut alerts, "archive_copy_superseded").await;
    tokio::time::sleep(10 * FINALITY_EVERY).await;
    assert!(!drained(&mut alerts, "archive_object_mismatch"), "no range of the superseded tail is read back");
    assert_eq!((metrics.archive.mismatches.get(), bucket.with(|b| b.created)), (0, 0));

    // The copy that replaces it holds the vault's history, the bucket's object for the second range among it.
    bucket.with(|b| {
        b.objects.insert(range_key(&test_log(), 1), compress(&range_bytes(&dir, &view, 1).unwrap()).unwrap())
    });
    vault.finalize(seq, &root);
    sources.send_replace(Some(node.manifest.clone()));
    recv(&mut resolved, "archive_copy_superseded").await;
    let below = seq / RANGE_SEQS;
    until("the ranges below the checkpoint uploaded", || bucket.with(|b| b.created) >= below - 2).await;
    assert_eq!(metrics.archive.mismatches.get(), 0);
    running.abort();
    node.stop().await;
}

/// A finalized checkpoint the copy holds only in its open segment is taken as the bound once that segment is
/// completed, and never read from the open segment.
#[tokio::test]
async fn a_checkpoint_in_the_open_segment_waits_for_the_segment() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-open-checkpoint"));
    let node = node_with(&dir, 3).await;
    let (seq, root) = loop {
        let completed = node.manifest.borrow().unlisted_from().unwrap();
        if let Some(open) = checkpoints(&dir).into_iter().find(|(seq, _)| *seq > completed) {
            break open;
        }
        node.submit(Command::Tick).await;
    };
    let vault = Vault::default();
    vault.finalize(seq, &root);
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), Some(vault.clone()));
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    until("the whole ranges counted", || metrics.archive.pending_ranges.get() > 0).await;
    tokio::time::sleep(10 * FINALITY_EVERY).await;
    assert_eq!(bucket.with(|b| b.created), 0, "the checkpoint is not taken from the open segment");
    while node.manifest.borrow().unlisted_from().unwrap() <= seq {
        node.submit(Command::Tick).await;
    }
    let below = (seq / RANGE_SEQS) as i64;
    until("the ranges below the checkpoint confirmed", || metrics.archive.confirmed_ranges.get() >= below).await;
    running.abort();
    node.stop().await;
}

/// A vault that cannot be read is a failure, retried with the rest, never a bound.
#[tokio::test]
async fn a_vault_that_cannot_be_read_is_retried() {
    let (bucket, dir) = (StandIn::start().await, scratch_dir("archive-vault-down"));
    let node = node_with(&dir, 2).await;
    let vault = Vault::default();
    vault.fail_next(3);
    vault.finalize(2 * RANGE_SEQS - 1, &root_at(&dir, 2 * RANGE_SEQS - 1));
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), Some(vault.clone()));
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    until("the first range confirmed", || metrics.archive.confirmed_ranges.get() == 1).await;
    assert!(vault.reads() >= 4 && metrics.archive.failures.get() >= 3, "each failed read retried");
    running.abort();
    node.stop().await;
}

/// The root of the checkpoint at `seq` in the log in `dir`.
fn root_at(dir: &Path, seq: u64) -> String {
    let found = checkpoints(dir).into_iter().find(|(at, _)| *at == seq);
    found.unwrap_or_else(|| panic!("a checkpoint at seq {seq}")).1
}
