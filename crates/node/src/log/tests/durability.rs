//! Writer durability and rotation: per-batch sync, reopened segments, staging, and opening the log
//! while a writer rotates.

use super::*;

#[test]
fn a_flush_syncs_the_open_segment_only_after_an_append() {
    // Rotation replaces anything at its staging name, so the open segment is pointed at
    // `/dev/null`, which cannot be synced, directly: a flush fails exactly when it syncs.
    let dir = scratch_dir("flush-sync");
    let mut writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(10, watch::channel(Manifest::default()).0).unwrap();
    writer.append(&cmd(0)).unwrap();
    writer.flush().unwrap();
    let (segment, _) = writer.open.take().unwrap();
    let null = fs::OpenOptions::new().write(true).open("/dev/null").unwrap();
    writer.open = Some((segment, std::io::BufWriter::new(null)));
    writer.append(&cmd(1)).unwrap();
    let err = writer.flush().unwrap_err();
    assert!(format!("{err:#}").contains("syncing log"), "{err:#}");

    // With nothing appended since the last sync, a flush does not sync.
    writer.unsynced = false;
    writer.flush().unwrap();
}

#[test]
fn a_reopened_segment_is_synced_before_its_entries_count_as_flushed() {
    // A process that died after writing entries, before syncing them, leaves them in the page cache,
    // where a power loss drops them. A writer reopening the segment syncs it, and starts it marked
    // unsynced, so its first flush syncs it again before publishing a cursor.
    let dir = scratch_dir("reopen-unsynced");
    let mut writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(10, watch::channel(Manifest::default()).0).unwrap();
    writer.append(&cmd(0)).unwrap();
    writer.flush().unwrap();
    drop(writer);
    let writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(10, watch::channel(Manifest::default()).0).unwrap();
    assert!(writer.unsynced, "the reopened segment counts as unsynced");
    drop(writer);
    // Reopened through a link to `/dev/null`, a segment holding entries fails the recovered-append
    // length check before any sync, and the writer does not start.
    let log = SegmentedLog::open(&dir, |_| Ok(())).unwrap();
    let segment = dir.join(SEG_0);
    fs::remove_file(&segment).unwrap();
    std::os::unix::fs::symlink("/dev/null", &segment).unwrap();
    let error = log.into_writer(10, watch::channel(Manifest::default()).0).err().expect("the writer does not start");
    assert!(format!("{error:#}").contains("length changed before append"), "{error:#}");
}

#[test]
fn a_reopened_segment_that_cannot_be_synced_does_not_start_the_writer() {
    // An empty open segment matches `/dev/null` in length and hash, so reopening it reaches the
    // sync itself: a character device takes the open but cannot satisfy it. Deleting the
    // reopened-segment sync starts this writer, and the test fails.
    let dir = scratch_dir("reopen-empty-null");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(SEG_0), b"").unwrap();
    let log = SegmentedLog::open(&dir, |_| Ok(())).unwrap();
    fs::remove_file(dir.join(SEG_0)).unwrap();
    std::os::unix::fs::symlink("/dev/null", dir.join(SEG_0)).unwrap();
    let error = log.into_writer(10, watch::channel(Manifest::default()).0).err().expect("the writer does not start");
    assert!(format!("{error:#}").contains("syncing"), "{error:#}");
}

#[test]
fn a_synced_flush_leaves_nothing_to_sync() {
    let dir = scratch_dir("synced");
    let mut writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(10, watch::channel(Manifest::default()).0).unwrap();
    writer.append(&cmd(0)).unwrap();
    writer.flush().unwrap();
    assert!(!writer.unsynced);
}

#[test]
fn a_reopened_writer_starts_unsynced_until_its_first_sync() {
    // A segment an earlier process wrote may hold bytes no sync of this writer covers: the reopened
    // writer starts unsynced, so a flush with no new append still syncs before publishing a cursor.
    let dir = scratch_dir("reopen-flag");
    write(&dir, 2, &[cmd(0)]);
    let mut writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, watch::channel(Manifest::default()).0).unwrap();
    assert!(writer.unsynced, "the reopened open segment may hold unsynced bytes");
    writer.flush().unwrap();
    assert!(!writer.unsynced, "the flush with no new append synced the reopened segment");

    // With every segment listed there is no open segment that could hold unsynced bytes.
    let dir = scratch_dir("reopen-listed");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    fs::remove_file(dir.join(SEG_2)).unwrap();
    let writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, watch::channel(Manifest::default()).0).unwrap();
    assert!(!writer.unsynced);
}

/// History repair opens the log while the writer rotates. Rotation lists a segment before it links
/// the next, so a listing taken first is covered by the manifest read after it, however many
/// rotations fall between; a startup open, with no writer, still refuses a manifest listing a
/// missing segment.
#[test]
fn a_segment_listing_taken_before_rotations_opens_with_the_newer_manifest_while_writing() {
    let dir = scratch_dir("open-listing-before-rotation");
    let mut writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, watch::channel(Manifest::default()).0).unwrap();
    for seq in 0..3 {
        writer.append(&cmd(seq)).unwrap();
    }
    writer.flush().unwrap();
    let listed = super::segments::list_segments(&dir).unwrap();
    for seq in 3..7 {
        writer.append(&cmd(seq)).unwrap();
    }
    writer.flush().unwrap();
    assert!(SegmentedLog::open_listed(&dir, listed.clone(), false, |_| Ok(())).is_err());
    let mut seen = Vec::new();
    SegmentedLog::open_listed(&dir, listed, true, |entry| {
        seen.push(entry);
        Ok(())
    })
    .expect("the newer manifest covers the older listing");
    assert_eq!(seen, [cmd(0), cmd(1), cmd(2), cmd(3)]);
}

/// Rotation stages each segment under one name. A crash before the link leaves it for the next
/// attempt at that rotation to replace; a crash after the link leaves it beside the active segment,
/// where opening the writer removes it. Neither lists the log directory.
#[test]
fn rotation_staging_a_crash_leaves_is_replaced_or_removed_without_listing_the_log() {
    let dir = scratch_dir("segment-staging-left");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    let linked = dir.join(format!("{SEG_2}.tmp"));
    fs::hard_link(dir.join(SEG_2), &linked).unwrap();
    let next = "00000000000000000004.jsonl";
    fs::write(dir.join(format!("{next}.tmp")), b"interrupted").unwrap();
    let unrelated = dir.join("notes.tmp");
    fs::write(&unrelated, b"keep").unwrap();
    let mut writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, watch::channel(Manifest::default()).0).unwrap();
    assert!(!linked.exists(), "the active segment's staging name is removed");
    for seq in 3..5 {
        writer.append(&cmd(seq)).unwrap();
    }
    writer.flush().unwrap();
    assert!(dir.join(next).exists(), "the rotation replaced its interrupted staging file");
    assert!(!dir.join(format!("{next}.tmp")).exists());
    assert!(unrelated.exists(), "nothing else is touched");
    assert_eq!(log_entries(&dir), (0..5).map(cmd).collect::<Vec<_>>());
}

/// With seals deferred, rotation leaves the old segment flushed but unlisted and the new one at its
/// staging name; the sync job carrying the seal lists the old segment durably, links the new one,
/// and only then publishes both, in order.
#[test]
fn a_deferred_seal_publishes_only_with_its_sync() {
    let dir = scratch_dir("deferred-seal");
    let (published, view) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, published).unwrap();
    writer.defer_seals();
    for seq in 0..3 {
        writer.append(&cmd(seq)).unwrap();
    }
    assert_eq!(Manifest::load(&dir).unwrap().segments.len(), 0, "the seal has not listed the segment yet");
    assert_eq!(file_entries(&dir.join(SEG_0)), [cmd(0), cmd(1)], "the old segment is flushed");
    assert!(!dir.join(SEG_2).exists(), "the new segment has not taken its name");
    assert!(dir.join(format!("{SEG_2}.tmp")).exists());

    let job = writer.begin_sync(false).unwrap();
    job.sync().unwrap();
    writer.finish_sync(&job);

    let ranges: Vec<_> = Manifest::load(&dir).unwrap().segments.iter().map(|s| (s.first_seq, s.last_seq)).collect();
    assert_eq!(ranges, [(0, 1)]);
    assert!(dir.join(SEG_2).exists() && !dir.join(format!("{SEG_2}.tmp")).exists());
    assert_eq!(log_entries(&dir), (0..3).map(cmd).collect::<Vec<_>>());
    assert_eq!(view.borrow().completed_len(), 1);
    let active = view.borrow().active.clone().expect("the new segment is active once its seal has run");
    assert_eq!((active.info.first_seq, active.last.seq), (2, 2));
}

/// Two rotations before any sync carry two seals on one job: each listing lands after the pending
/// seals' lines, and both staged files link, in rotation order.
#[test]
fn two_deferred_seals_ride_one_sync_in_order() {
    let dir = scratch_dir("deferred-seal-chain");
    let (published, view) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, published).unwrap();
    writer.defer_seals();
    for seq in 0..5 {
        writer.append(&cmd(seq)).unwrap();
    }
    assert_eq!(Manifest::load(&dir).unwrap().segments.len(), 0);

    let job = writer.begin_sync(false).unwrap();
    job.sync().unwrap();
    writer.finish_sync(&job);

    let ranges: Vec<_> = Manifest::load(&dir).unwrap().segments.iter().map(|s| (s.first_seq, s.last_seq)).collect();
    assert_eq!(ranges, [(0, 1), (2, 3)]);
    assert!(dir.join("00000000000000000004.jsonl").exists());
    assert_eq!(log_entries(&dir), (0..5).map(cmd).collect::<Vec<_>>());
    assert_eq!(view.borrow().completed_len(), 2);
    assert_eq!(view.borrow().active.as_ref().unwrap().info.first_seq, 4);
}

/// A process that dies after a deferred rotation but before the seal's sync leaves the old segment
/// complete and unlisted with the new one staged — exactly what a crash mid-rotation leaves today —
/// and reopening resumes from the old segment, replacing the staging file at its next rotation.
#[test]
fn a_deferred_seal_left_by_a_crash_recovers_as_a_crash_mid_rotation() {
    let dir = scratch_dir("deferred-seal-crash");
    {
        let mut writer = SegmentedLog::open(&dir, |_| Ok(()))
            .unwrap()
            .into_writer(2, watch::channel(Manifest::default()).0)
            .unwrap();
        writer.defer_seals();
        for seq in 0..3 {
            writer.append(&cmd(seq)).unwrap();
        }
    }
    assert_eq!(Manifest::load(&dir).unwrap().segments.len(), 0);
    assert!(!dir.join(SEG_2).exists());

    let mut writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, watch::channel(Manifest::default()).0).unwrap();
    writer.defer_seals();
    for seq in 2..4 {
        writer.append(&cmd(seq)).unwrap();
        let job = writer.begin_sync(false).unwrap();
        job.sync().unwrap();
        writer.finish_sync(&job);
    }
    let ranges: Vec<_> = Manifest::load(&dir).unwrap().segments.iter().map(|s| (s.first_seq, s.last_seq)).collect();
    assert_eq!(ranges, [(0, 1)], "the reopened segment lists at its rotation; the last stays open");
    assert!(dir.join(SEG_2).exists() && !dir.join(format!("{SEG_2}.tmp")).exists());
    assert_eq!(log_entries(&dir), (0..4).map(cmd).collect::<Vec<_>>());
}

/// A second seal deferred while the first rides an unfinished sync job starts its listing after the
/// first's: the manifest length counts seals already taken into a job, not only queued ones, so
/// both listings land in order — the two-consecutive-batch-crossings case a busy node hits at
/// nearly every rotation.
#[test]
fn a_deferred_seal_counts_a_seal_riding_an_unfinished_sync() {
    let dir = scratch_dir("deferred-seal-in-flight");
    let (published, view) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, published).unwrap();
    writer.defer_seals();
    for seq in 0..3 {
        writer.append(&cmd(seq)).unwrap();
    }
    // The first batch's job carries the first seal but has not run.
    let first = writer.begin_sync(false).unwrap();
    // The next batch crosses another boundary before its predecessor's sync finishes.
    for seq in 3..5 {
        writer.append(&cmd(seq)).unwrap();
    }
    let second = writer.begin_sync(false).unwrap();
    first.sync().unwrap();
    writer.finish_sync(&first);
    second.sync().unwrap();
    writer.finish_sync(&second);

    let ranges: Vec<_> = Manifest::load(&dir).unwrap().segments.iter().map(|s| (s.first_seq, s.last_seq)).collect();
    assert_eq!(ranges, [(0, 1), (2, 3)], "both listings land in rotation order");
    assert!(dir.join("00000000000000000004.jsonl").exists());
    assert_eq!(log_entries(&dir), (0..5).map(cmd).collect::<Vec<_>>());
    assert_eq!(view.borrow().completed_len(), 2);
    assert_eq!(view.borrow().active.as_ref().unwrap().info.first_seq, 4);
}

/// A job carrying a seal is never covered by a later same-segment job: group commit may skip a
/// batch's data sync, never its segment seal.
#[test]
fn a_sync_job_carrying_a_seal_is_not_covered_by_a_later_one() {
    let dir = scratch_dir("deferred-seal-covered");
    let mut writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, watch::channel(Manifest::default()).0).unwrap();
    writer.defer_seals();
    for seq in 0..3 {
        writer.append(&cmd(seq)).unwrap();
    }
    let sealed = writer.begin_sync(false).unwrap();
    assert!(!sealed.is_empty(), "the job carries the deferred seal");
    writer.append(&cmd(3)).unwrap();
    let plain = writer.begin_sync(false).unwrap();
    assert!(!sealed.covered_by(&plain), "a seal must run even when a later job syncs the same segment");
}

/// A direct flush while a seal pends publishes no active prefix: the view still holds the previous
/// segment as active until the seal's sync lists it and publishes the completion.
#[test]
fn a_flush_while_a_seal_pends_publishes_no_active_prefix() {
    let dir = scratch_dir("deferred-seal-flush");
    let (published, view) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, published).unwrap();
    writer.defer_seals();
    for seq in 0..3 {
        writer.append(&cmd(seq)).unwrap();
    }
    writer.flush().unwrap();
    assert_eq!(view.borrow().flushed, Some(Offset { seq: 2, index: 0 }));
    assert!(view.borrow().active.is_none(), "the new segment is not active before its seal publishes");

    let job = writer.begin_sync(false).unwrap();
    job.sync().unwrap();
    writer.finish_sync(&job);
    assert_eq!(view.borrow().active.as_ref().unwrap().info.first_seq, 2);
}
