//! A segment the host must keep and lacks is put back from the archive, and one no source gives back pages; a whole
//! replay first puts back every segment the window removed.

use super::*;

use crate::archive::read::origin::{copy_log, serve};
use crate::archive::stand_in::test_log;
use crate::config::DailySnapshotsSection;
use crate::test_support::capture_alerts;

/// A segment after the oldest on the host that the host lost, which a restart's read of the log refuses, is put back
/// from the bucket as its listing names it.
#[tokio::test]
async fn a_lost_segment_after_the_oldest_on_the_host_is_put_back_from_the_bucket() {
    let fixture = Fixture::start("window-hole", 60).await;
    let ranges = fixture.archive_all();
    let view = fixture.view();
    let lost = view.segments[3].clone();
    let bytes = std::fs::read(fixture.dir.join(&lost.file)).unwrap();
    std::fs::remove_file(fixture.dir.join(&lost.file)).unwrap();
    let (mut window, _archived) = fixture.window((600, 30), ranges);
    let pass = window.pass(&view).await.unwrap();
    assert_eq!(pass.restored, vec![lost.file.clone()]);
    assert_eq!(std::fs::read(fixture.dir.join(&lost.file)).unwrap(), bytes, "the bytes its listing names");
    assert_eq!(window.metrics.archive.window.restored.get(), 1);
    let staged = std::fs::read_dir(&fixture.dir).unwrap().flatten();
    assert!(!staged.into_iter().any(|item| item.file_name().to_string_lossy().contains(".restoring.")), "none staged");
    log_entries(&fixture.dir);
    fixture.node.stop().await;
}

/// A segment the uploader still needs, which the host lost before the bucket held it, pages Critical
/// `log_segment_missing` once; once another copy of the log puts its ranges in the bucket, it is put back from there
/// and the alert resolves.
#[tokio::test]
async fn a_lost_segment_the_bucket_lacks_pages_until_another_copy_uploads_it() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let fixture = Fixture::start("window-lost", 60).await;
    let view = fixture.view();
    let copy = fixture.dir.parent().unwrap().join("copy");
    copy_log(&fixture.dir, &copy);
    let lost = view.segments[view.segments.len() - 2].clone();
    std::fs::remove_file(fixture.dir.join(&lost.file)).unwrap();
    let (mut window, _archived) = fixture.window((600, 30), 0);
    for _ in 0..2 {
        let pass = window.pass(&view).await.unwrap();
        assert_eq!(pass.restored, Vec::<String>::new());
    }
    let alert = named(&mut alerts, "log_segment_missing").await;
    assert!(alert["message"].contains(&lost.file), "{alert:?}");
    assert!(!drained(&mut alerts, "log_segment_missing"), "paged once");
    // A member's copy, which holds the segment, uploads its ranges.
    upload(&fixture.bucket, &copy, &view, whole_ranges(&view).unwrap());
    let pass = window.pass(&view).await.unwrap();
    assert_eq!(pass.restored, vec![lost.file.clone()]);
    named(&mut resolved, "log_segment_missing").await;
    assert!(held(&fixture.dir, &lost));
    fixture.node.stop().await;
}

/// The oldest segments, lost before the bucket held them, come back newest first from a node origin that serves them,
/// so the segments on the host stay one run; one no source gives back stops the run there.
#[tokio::test]
async fn lost_oldest_segments_come_back_newest_first_from_a_node_origin() {
    let _guard = capture_alerts();
    let fixture = Fixture::start("window-lost-oldest", 60).await;
    let view = fixture.view();
    let served = fixture.dir.parent().unwrap().join("served");
    copy_log(&fixture.dir, &served);
    let lost: Vec<SegmentInfo> = view.segments[..3].to_vec();
    for listed in &lost {
        std::fs::remove_file(fixture.dir.join(&listed.file)).unwrap();
    }
    // The origin serves the second and third, not the first.
    std::fs::remove_file(served.join(&lost[0].file)).unwrap();
    let origin = serve(&served, None).await;
    let (window, _archived) = fixture.window((600, 30), 0);
    let mut window =
        Window { restore: RestoreSources { nodes: vec![origin.url.clone()], ..window.restore.clone() }, ..window };
    let pass = window.pass(&view).await.unwrap();
    assert_eq!(pass.restored, vec![lost[2].file.clone(), lost[1].file.clone()], "newest first");
    assert!(!held(&fixture.dir, &lost[0]) && held(&fixture.dir, &lost[1]));
    log_entries_from(&fixture.dir, lost[1].first_seq);
    fixture.node.stop().await;
}

/// A whole replay of a log the window trimmed reads every segment, which the archive puts back first; a log whose
/// segments are all on the host is left as it is, and a node with no `[archive]` puts nothing back.
#[tokio::test]
async fn a_whole_replay_first_puts_back_every_segment_the_window_removed() {
    let fixture = Fixture::start("window-whole", 150).await;
    let ranges = fixture.archive_all();
    fixture.snapshot_at(100);
    let (mut window, _archived) = fixture.window((30, 30), ranges);
    let removed = window.pass(&fixture.view()).await.unwrap().removed;
    assert!(!removed.is_empty());
    fixture.node.stop().await;
    let mut cfg = dev_config();
    cfg.node.log_dir = fixture.dir.clone();
    assert_eq!(restore_whole(&cfg, &fixture.dir).await.unwrap(), 0, "no [archive], nothing to read");
    let mut section = fixture.bucket.section(Duration::from_secs(3600));
    section.public_url = fixture.bucket.plain.clone();
    cfg.archive = Some(section);
    cfg.daily_snapshots = Some(DailySnapshotsSection::at(fixture.snapshots.clone()));
    assert_eq!(crate::archive::log_prefix(cfg.custody.chain_id, cfg.custody.vault.parse().unwrap()), test_log());
    assert_eq!(restore_whole(&cfg, &fixture.dir).await.unwrap(), removed.len() as u64);
    assert!(removed.iter().all(|file| fixture.dir.join(file).exists()));
    log_entries(&fixture.dir);
    assert_eq!(restore_whole(&cfg, &fixture.dir).await.unwrap(), 0, "a whole log is left as it is");
}

/// A whole restore that stops part way, here at a segment the archive no longer gives back, has put back the segments
/// after it, newest first, so the host's segments are still one run that reads.
#[tokio::test]
async fn an_interrupted_whole_restore_leaves_one_run_of_segments() {
    let fixture = Fixture::start("window-whole-interrupted", 150).await;
    let ranges = fixture.archive_all();
    fixture.snapshot_at(100);
    let (mut window, _archived) = fixture.window((30, 30), ranges);
    let removed = window.pass(&fixture.view()).await.unwrap().removed;
    assert!(removed.len() >= 4, "{removed:?}");
    let view = fixture.view();
    fixture.node.stop().await;
    let first = view.segments.iter().position(|listed| listed.file == removed[0]).unwrap();
    let key = crate::archive::range_key(&test_log(), view.segments[first].first_seq / range_seqs());
    fixture.bucket.with(|b| b.objects.remove(&key));
    let mut cfg = dev_config();
    let mut section = fixture.bucket.section(Duration::from_secs(3600));
    section.public_url = fixture.bucket.plain.clone();
    (cfg.archive, cfg.node.log_dir) = (Some(section), fixture.dir.clone());
    restore_whole(&cfg, &fixture.dir).await.expect_err("a range the bucket no longer holds");
    let newest = view.segments.iter().find(|listed| listed.file == *removed.last().unwrap()).unwrap();
    assert!(held(&fixture.dir, newest), "the newest it lacked is back");
    let local = list_segments(&fixture.dir).unwrap()[0].0;
    assert!(local > view.segments[first].first_seq, "short of the oldest");
    log_entries_from(&fixture.dir, local);
}

/// Lost segments no source gives back are tried one a pass, the newest first, and page one `log_segment_missing` for
/// it, however many there are; once the bucket is back, every one is put back and the alert resolves.
#[tokio::test]
async fn lost_segments_no_source_gives_back_are_tried_and_paged_one_at_a_time() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let fixture = Fixture::start("window-holes", 60).await;
    let ranges = fixture.archive_all();
    let view = fixture.view();
    let lost: Vec<SegmentInfo> = [2, 4, 6].iter().map(|at| view.segments[*at].clone()).collect();
    lost.iter().for_each(|listed| std::fs::remove_file(fixture.dir.join(&listed.file)).unwrap());
    fixture.bucket.with(|b| b.down = true);
    let (mut window, _archived) = fixture.window((600, 30), ranges);
    for _ in 0..2 {
        assert_eq!(window.pass(&view).await.unwrap().restored, Vec::<String>::new());
    }
    let alert = named(&mut alerts, "log_segment_missing").await;
    assert!(alert["message"].contains(&lost[2].file), "the newest the host lacks: {alert:?}");
    assert!(!drained(&mut alerts, "log_segment_missing"), "paged once");
    fixture.bucket.with(|b| b.down = false);
    let restored = window.pass(&view).await.unwrap().restored;
    assert_eq!(restored, lost.iter().rev().map(|listed| listed.file.clone()).collect::<Vec<_>>());
    named(&mut resolved, "log_segment_missing").await;
    log_entries(&fixture.dir);
    fixture.node.stop().await;
}

/// Every entry of the log in `dir` from the segment starting at `first_seq` on.
fn log_entries_from(dir: &Path, first_seq: u64) -> Vec<LogEntry> {
    let mut entries = Vec::new();
    crate::log::SegmentedLog::open_from(dir, Offset::start_of(first_seq), |entry| {
        entries.push(entry);
        Ok(())
    })
    .expect("the log opens from there");
    entries
}

/// Until the uploader has found what the bucket holds, as after a restart, the window removes nothing and puts back only
/// a segment after the oldest on the host; and a count a stale `404` at the uploader's resume lowered puts back no
/// segment whose ranges the bucket holds.
#[tokio::test]
async fn a_restart_puts_back_nothing_the_bucket_holds() {
    let fixture = Fixture::start("window-restart", 150).await;
    let ranges = fixture.archive_all();
    fixture.snapshot_at(100);
    let (mut window, archived) = fixture.window((30, 30), ranges);
    let view = fixture.view();
    let removed = window.pass(&view).await.unwrap().removed;
    assert!(!removed.is_empty());
    archived.send_replace(None);
    assert_eq!(window.pass(&view).await.unwrap(), Pass::default(), "nothing while the count is unknown");
    // A segment after the oldest on the host, which a restart's read needs whatever the bucket holds, comes back.
    let hole = view.segments.iter().filter(|listed| held(&fixture.dir, listed)).nth(1).unwrap().clone();
    std::fs::remove_file(fixture.dir.join(&hole.file)).unwrap();
    let pass = window.pass(&view).await.unwrap();
    assert_eq!((pass.restored, pass.removed), (vec![hole.file.clone()], vec![]), "and only that");
    archived.send_replace(Some(0));
    let pass = window.pass(&view).await.unwrap();
    assert_eq!((pass.restored, pass.held), (Vec::<String>::new(), Some(Held::Archive)));
    assert!(removed.iter().all(|file| !fixture.dir.join(file).exists()), "the bucket holds every one");
    fixture.node.stop().await;
}

/// A whole replay puts back only a segment named for its first seq: a manifest the host altered to name a path
/// outside the log directory is refused before anything is written.
#[tokio::test]
async fn a_whole_replay_writes_no_listing_not_named_for_its_first_seq() {
    let fixture = Fixture::start("window-whole-names", 60).await;
    let ranges = fixture.archive_all();
    let view = fixture.view();
    fixture.node.stop().await;
    let first = view.segments[0].clone();
    std::fs::remove_file(fixture.dir.join(&first.file)).unwrap();
    let manifest = std::fs::read_to_string(fixture.dir.join(crate::log::MANIFEST)).unwrap();
    let outside = "../outside.jsonl";
    std::fs::write(fixture.dir.join(crate::log::MANIFEST), manifest.replacen(&first.file, outside, 1)).unwrap();
    let mut cfg = dev_config();
    let mut section = fixture.bucket.section(Duration::from_secs(3600));
    section.public_url = fixture.bucket.plain.clone();
    (cfg.archive, cfg.node.log_dir) = (Some(section), fixture.dir.clone());
    assert!(ranges > 0);
    let refused = restore_whole(&cfg, &fixture.dir).await.expect_err("a listing not named for its first seq");
    assert!(format!("{refused:#}").contains("not the canonical segment name"), "{refused:#}");
    assert!(!fixture.dir.parent().unwrap().join("outside.jsonl").exists());
}

/// A window that starts following removes what an earlier process's crash left staged, whatever process id that ran
/// under, and leaves this process's own, which a restore beside it may be writing.
#[test]
fn a_window_removes_only_an_earlier_processs_staged_restores() {
    let dir = scratch_dir("window-staged");
    std::fs::create_dir_all(&dir).unwrap();
    let file = crate::log::segment_name(4);
    let earlier = format!("{file}.restoring.{:016x}.0", std::process::id());
    std::fs::write(dir.join(&earlier), b"part").unwrap();
    let mine = crate::archive::window::restore::staged(&dir, &file);
    std::fs::write(&mine, b"part").unwrap();
    crate::archive::window::restore::remove_staged(&dir).unwrap();
    assert!(!dir.join(&earlier).exists() && mine.exists(), "an earlier run's goes, as under the same process id");
}

/// A segment the floors keep that the host lacks comes back, newest first, while the one after it is on the host:
/// here the daily snapshot at the history's floor leaves the host, the floor moves back to the one before it, and the
/// segments from there to the oldest on the host come back from the bucket, and stay.
#[tokio::test]
async fn segments_the_floors_keep_come_back_once_a_floor_moves_back() {
    let fixture = Fixture::start("window-floor-back", 160).await;
    let ranges = fixture.archive_all();
    let early = fixture.snapshot_at(60);
    let floor = fixture.snapshot_at(100);
    fixture.snapshot_at(140);
    let (mut window, _archived) = fixture.window((30, 30), ranges);
    assert!(!window.pass(&fixture.view()).await.unwrap().removed.is_empty());
    let first_local = list_segments(&fixture.dir).unwrap()[0].0;
    assert!(early < first_local && first_local <= floor, "the window kept the log from the floor's snapshot");
    let listed = crate::daily_snapshots::read_manifest(&fixture.snapshots).unwrap();
    std::fs::remove_file(fixture.snapshots.join(&listed.iter().find(|l| l.seq == floor).unwrap().file)).unwrap();
    let pass = window.pass(&fixture.view()).await.unwrap();
    assert!(!pass.restored.is_empty() && pass.removed.is_empty(), "{pass:?}");
    let seqs: Vec<u64> = pass.restored.iter().map(|file| crate::log::segment_seq(file).unwrap()).collect();
    assert!(seqs.windows(2).all(|pair| pair[0] > pair[1]), "newest first: {seqs:?}");
    let first_local = list_segments(&fixture.dir).unwrap()[0].0;
    let holding = fixture.view().segments.iter().find(|l| l.first_seq == first_local).unwrap().clone();
    assert!(holding.first_seq <= early && early <= holding.last_seq, "back to the segment of the snapshot before");
    assert_eq!(window.pass(&fixture.view()).await.unwrap().restored, Vec::<String>::new(), "and they stay");
    commands(&fixture.dir);
    fixture.node.stop().await;
}
