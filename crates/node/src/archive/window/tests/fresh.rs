//! Where a new history index takes the log from once the log's oldest segments have left the host.

use super::*;
use crate::config::DailySnapshotsSection;
use crate::history::HistoryPlan;
use crate::test_support::{RETENTION, TestHistory};

/// While the host holds the log's first segment, a new index takes the whole log; once the window has removed the
/// oldest, it takes it from the command after the daily snapshot before the history's retention, whose rows it
/// re-derives from the log on the host, or from the log's first command with no such snapshot.
#[tokio::test]
async fn a_new_history_index_starts_where_the_host_holds_what_it_re_derives() {
    let fixture = Fixture::start("window-fresh", 150).await;
    let ranges = fixture.archive_all();
    let at_retention = fixture.snapshot_at(100);
    let retention = Duration::from_secs(30 * 60);
    let fresh = |snapshots: Option<&Path>| fresh_start(&fixture.view(), &fixture.dir, snapshots, retention).unwrap();
    assert_eq!(fresh(Some(&fixture.snapshots)), None, "the host holds the whole log");
    let (mut window, _archived) = fixture.window((30, 30), ranges);
    assert!(!window.pass(&fixture.view()).await.unwrap().removed.is_empty());
    let first_local = fixture.view().segments.iter().find(|listed| held(&fixture.dir, listed)).unwrap().first_seq;
    assert!(first_local <= at_retention, "the window kept the newest snapshot's checkpoint");
    let from = |from| Some(FreshStart { from, first_local });
    assert_eq!(fresh(Some(&fixture.snapshots)), from(at_retention + 1), "after the snapshot before the retention");
    assert_eq!(fresh(None), from(0), "with no snapshots, the log's first command");
    fixture.node.stop().await;
}

/// After `history.retention` grew past what the window kept, an index that rebuilds takes the log from the command
/// after the snapshot before its new retention, which the archive puts back first, newest first, or from genesis once
/// that snapshot's file is gone too; one that resumes takes nothing back.
#[tokio::test]
async fn a_rebuilt_index_whose_retention_grew_puts_back_the_log_it_takes() {
    let fixture = Fixture::start("window-fresh-grown", 150).await;
    let ranges = fixture.archive_all();
    let early = fixture.snapshot_at(20);
    fixture.snapshot_at(100);
    let (mut window, _archived) = fixture.window((30, 30), ranges);
    assert!(!window.pass(&fixture.view()).await.unwrap().removed.is_empty());
    let first_local = list_segments(&fixture.dir).unwrap()[0].0;
    assert!(early < first_local, "the window removed the log after the early snapshot");
    let mut cfg = dev_config();
    let mut section = fixture.bucket.section(Duration::from_secs(3600));
    section.public_url = fixture.bucket.plain.clone();
    (cfg.archive, cfg.node.log_dir) = (Some(section), fixture.dir.clone());
    cfg.daily_snapshots = Some(DailySnapshotsSection::at(fixture.snapshots.clone()));
    cfg.history.retention = Duration::from_secs(100 * 60);
    let view = fixture.view();
    let start = history_start(&cfg, &view, |_| true).await.unwrap();
    assert_eq!(start, Some(first_local), "an index that resumes takes nothing back");
    assert_eq!(list_segments(&fixture.dir).unwrap()[0].0, first_local);
    let start = history_start(&cfg, &view, |_| false).await.unwrap();
    assert_eq!(start, Some(early + 1), "a rebuilt one, from the snapshot before its retention");
    let back = view.segments.iter().find(|listed| listed.last_seq > early).unwrap();
    assert_eq!(list_segments(&fixture.dir).unwrap()[0].0, back.first_seq, "put back from there");
    commands(&fixture.dir);
    // With the early snapshot's file gone from the host too, as the daily snapshots' own window removes it, the
    // rebuilt index takes the whole log, whose re-derivations replay from genesis.
    let listed = crate::daily_snapshots::read_manifest(&fixture.snapshots).unwrap();
    std::fs::remove_file(fixture.snapshots.join(&listed.iter().find(|l| l.seq == early).unwrap().file)).unwrap();
    assert_eq!(history_start(&cfg, &view, |_| false).await.unwrap(), Some(0), "from the log's first command");
    assert_eq!(list_segments(&fixture.dir).unwrap()[0].0, 0, "the whole log is back");
    fixture.node.stop().await;
}

/// In the minutes after a daily snapshot, when it is later than the newest completed segment's first command, the
/// fresh start counts the retention back from the snapshot, as the daily snapshots' own window does from the newest
/// listed, so it finds the one that window keeps before the retention rather than none and the log's start.
#[tokio::test]
async fn the_fresh_start_counts_the_retention_from_a_newer_snapshot() {
    let fixture = Fixture::start("window-fresh-newer", 150).await;
    let ranges = fixture.archive_all();
    let (early, (_, early_ts)) = (fixture.snapshot_at(20), fixture.at_minute(20));
    fixture.snapshot_at(100);
    let (mut window, _archived) = fixture.window((30, 30), ranges);
    assert!(!window.pass(&fixture.view()).await.unwrap().removed.is_empty());
    let newest_ts = commands(&fixture.dir).last().unwrap().1;
    assert!(newest_ts > fixture.now(), "the newest command is past the newest completed segment's first");
    fixture.snapshot_at((newest_ts - START) / MINUTE_MS);
    // A retention that reaches the early snapshot from the newest one, and not from the segment's first command.
    let retention = Duration::from_millis((newest_ts - early_ts - 1) as u64);
    let fresh = fresh_start(&fixture.view(), &fixture.dir, Some(&fixture.snapshots), retention).unwrap().unwrap();
    assert_eq!(fresh.from, early + 1, "after the snapshot the daily window keeps before the retention");
    fixture.node.stop().await;
}

/// A legacy index that holds nothing starts at the fresh start; one whose last command the log holds resumes after
/// it, unless it ends before the fresh start, past the retention, and is rebuilt from there.
#[tokio::test]
async fn a_legacy_index_that_ends_before_the_fresh_start_is_rebuilt_from_it() {
    let run = scratch_dir("window-fresh-index");
    let (dir, index) = (run.join("log"), run.join("history"));
    let node = TestSequencer::start(&dir, 256);
    for _ in 0..8 {
        node.submit(Command::Tick).await;
    }
    let history = TestHistory::start(&node, &dir, &index).await.unwrap();
    node.stop().await;
    history.finish().await.unwrap();
    let last = log_entries(&dir)
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(command) => Some(command),
            _ => None,
        })
        .next_back()
        .unwrap();
    let cfg = HistorySection { dir: index, retention: RETENTION, marks: None };
    let seq = last.sequenced.seq;
    let resume = |fresh: Option<u64>| {
        let mut plan = HistoryPlan::open(&cfg).unwrap();
        plan.observe(&LogEntry::Command(last.clone()));
        plan.fresh_from(fresh).finish().unwrap().resume_at()
    };
    assert_eq!(resume(None), Offset::start_of(seq + 1), "an index the log holds resumes after its last command");
    assert_eq!(resume(Some(seq + 1)), Offset::start_of(seq + 1), "one that ends at the fresh start resumes too");
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    assert_eq!(resume(Some(seq + 5)), Offset::start_of(seq + 5), "one that ends before it is rebuilt from it");
    assert_eq!(alerts.try_recv().unwrap()["alert"], "history_index_discarded");
    assert_eq!(resume(Some(seq + 5)), Offset::start_of(seq + 5), "and holding nothing, starts there");
}
