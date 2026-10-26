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
