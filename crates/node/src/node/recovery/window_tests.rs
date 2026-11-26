//! A start that reads segments the local window removed puts them back from the archive's bucket first: a whole replay,
//! with no daily snapshot to go on from, or with `[snapshots]` and no cache to take, whose history index and catalog
//! then rebuild from the whole log, and the binding of a posted head older than the links a daily snapshot carries.

use std::time::Duration;

use super::history_tests::Written;
use super::*;
use crate::archive::origin::upload;
use crate::archive::stand_in::StandIn;
use crate::archive::{RANGE_SEQS, whole_ranges};
use crate::test_support::published_manifest;

/// The written log's bucket, holding its whole ranges, and the oldest segments whose every range it holds, removed
/// from the host as the window removes them.
async fn trimmed(written: &Written) -> (StandIn, Vec<crate::log::SegmentInfo>) {
    let dir = written.cfg.node.log_dir.clone();
    let view = published_manifest(&dir).borrow().clone();
    let bucket = StandIn::start().await;
    let whole = whole_ranges(&view).unwrap();
    upload(&bucket, &dir, &view, whole);
    let removed: Vec<_> =
        view.segments.iter().filter(|listed| listed.last_seq < whole * RANGE_SEQS).take(3).cloned().collect();
    assert_eq!(removed.len(), 3, "the log holds segments the bucket holds");
    for listed in &removed {
        std::fs::remove_file(dir.join(&listed.file)).unwrap();
    }
    (bucket, removed)
}

/// `bucket` as `[archive]` names it, read over its plain origin.
fn section(bucket: &StandIn) -> crate::config::ArchiveSection {
    let mut section = bucket.section(Duration::from_secs(3600));
    section.public_url = bucket.plain.clone();
    section
}

#[tokio::test]
async fn a_start_with_no_daily_snapshot_puts_back_what_the_window_removed_and_replays_it() {
    let mut written = Written::run("window-whole-start").await;
    let dir = written.cfg.node.log_dir.clone();
    let (bucket, removed) = trimmed(&written).await;
    (written.cfg.daily_snapshots, written.cfg.archive) = (None, Some(section(&bucket)));
    let marks = marks::startup(&written.cfg, written.key.marks_key(), false).unwrap();
    let recovered = recover(&written.cfg, &written.key, None, &marks, None, &Metrics::new()).await.unwrap();
    assert_eq!(recovered.log.snapshot_sequence(), None, "the start replays the whole log");
    assert!(removed.iter().all(|listed| dir.join(&listed.file).exists()), "put back from the bucket");
}

#[tokio::test]
async fn a_paired_start_with_no_cache_puts_back_what_the_window_removed_and_rebuilds_from_it() {
    let mut written = Written::run("window-paired-start").await;
    let dir = written.cfg.node.log_dir.clone();
    let (bucket, removed) = trimmed(&written).await;
    let root = dir.parent().unwrap().to_owned();
    written.cfg.snapshots = Some(crate::config::SnapshotSection {
        dir: root.join("snapshots"),
        retain: 4.try_into().unwrap(),
        interval: Duration::ZERO,
        max_bytes: (16 * 1024 * 1024).try_into().unwrap(),
        trusted_signers: Vec::new(),
    });
    let marks = marks::startup(&written.cfg, written.key.marks_key(), false).unwrap();
    // Without [archive] the start cannot read the log it lacks, and refuses.
    let refused = recover(&written.cfg, &written.key, None, &marks, None, &Metrics::new()).await;
    let refused = refused.err().expect("a whole replay of a log that lacks its oldest segments");
    assert!(refused.is::<crate::log::NotLocal>(), "{refused:#}");
    drop(marks);
    written.cfg.archive = Some(section(&bucket));
    let marks = marks::startup(&written.cfg, written.key.marks_key(), false).unwrap();
    let recovered = recover(&written.cfg, &written.key, None, &marks, None, &Metrics::new()).await.unwrap();
    assert_eq!(recovered.log.snapshot_sequence(), None, "the start replays the whole log");
    assert!(removed.iter().all(|listed| dir.join(&listed.file).exists()), "put back from the bucket");
    let (log, _history) = recovered.history.finish_with_log(recovered.log, written.key.clone()).unwrap();
    assert!(log.manifest_view().catalog.is_some(), "the catalog rebuilt over every segment");
}

/// A start from a daily snapshot binds a posted head older than the links the snapshot carries by reading the log from
/// the head: on a log the window trimmed past the head, it puts those segments back from the archive first, and without
/// `[archive]` it refuses.
#[tokio::test]
async fn a_posted_head_older_than_the_snapshots_links_reads_its_segments_back_from_the_archive() {
    let mut written = Written::run("window-posted-head").await;
    let dir = written.cfg.node.log_dir.clone();
    let view = published_manifest(&dir).borrow().clone();
    let bucket = StandIn::start().await;
    upload(&bucket, &dir, &view, whole_ranges(&view).unwrap());
    let daily = written.cfg.daily_snapshots.as_ref().unwrap().dir.clone();
    let vault = written.cfg.custody.vault.parse().unwrap();
    let domain = crate::log::CheckpointDomain { chain_id: written.cfg.custody.chain_id, vault };
    let signers = [engine_core::types::AccountId(written.key.address())].into();
    let bounds = crate::daily_snapshots::Bounds { finalized: u64::MAX, before: None, keep_from: None };
    let roots = &crate::daily_snapshots::NoRoots;
    let mut start =
        crate::daily_snapshots::snapshot_start_against(None, (&daily, &dir), (domain, &signers), bounds, roots)
            .await
            .expect("the newest snapshot is usable");
    // The head at the log's first command, whose link the snapshot then no longer carries, as one past its links.
    let carried = start.snapshot.carried.as_mut().unwrap();
    let first = carried.links.remove(0);
    let head = crate::log_heads::Head { seq: first.seq, link: first.link };
    let removed: Vec<_> =
        view.segments.iter().filter(|listed| listed.last_seq < start.snapshot.offset.seq).cloned().collect();
    assert!(!removed.is_empty() && removed[0].first_seq <= head.seq, "the log holds the head before the snapshot");
    removed.iter().for_each(|listed| std::fs::remove_file(dir.join(&listed.file)).unwrap());
    let refused = under_daily(&written.cfg, Some(head), &start).await.expect_err("no archive to read the head from");
    assert!(refused.is::<crate::log::NotLocal>(), "{refused:#}");
    written.cfg.archive = Some(section(&bucket));
    let bound = under_daily(&written.cfg, Some(head), &start).await.expect("the head binds once its log is back");
    assert!(bound.is_some(), "the stamp after the head");
    assert!(removed.iter().all(|listed| dir.join(&listed.file).exists()), "put back from the bucket");
}
