//! The log's last checkpoint, read from its newest segment back: the one a whole read finds, reading
//! no segment before the newest that holds a checkpoint and the manifest only back from its end, each
//! segment it reads checked as a whole read checks it.

use super::*;

/// Wherever the last checkpoint sits, in the open segment, beside an earlier one in its segment, in a
/// listed segment behind two that hold none, or nowhere, the read finds the one a whole read finds.
#[test]
fn the_last_checkpoint_is_the_one_a_whole_read_finds() {
    let cases = [
        ("open", vec![cmd(0), checkpoint(0), cmd(1), cmd(2), checkpoint(2)]),
        ("beside", vec![cmd(0), checkpoint(0), cmd(1), checkpoint(1), cmd(2)]),
        ("behind", vec![cmd(0), cmd(1), checkpoint(1), cmd(2), cmd(3), cmd(4)]),
        ("none", vec![cmd(0), cmd(1), cmd(2)]),
        ("empty", vec![]),
    ];
    for (name, entries) in cases {
        let dir = scratch_dir(&format!("last-checkpoint-{name}"));
        write(&dir, 2, &entries);
        let mut whole = None;
        SegmentedLog::open(&dir, |entry| {
            if let LogEntry::Checkpoint(checkpoint) = entry {
                whole = Some(checkpoint);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(last_checkpoint(&dir).unwrap(), whole, "{name}");
    }
}

/// A segment before the newest one that holds a checkpoint is never read: one that no longer reads,
/// which a whole read refuses, leaves the read's answer as it was.
#[test]
fn no_segment_before_the_newest_checkpoint_is_read() {
    let dir = scratch_dir("last-checkpoint-bounded");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), checkpoint(2), cmd(3), cmd(4)]);
    fs::write(dir.join(SEG_0), b"not a log entry\n").unwrap();
    assert!(SegmentedLog::open(&dir, |_| Ok(())).is_err(), "a whole read refuses the first segment");
    let LogEntry::Checkpoint(expected) = checkpoint(2) else { unreachable!("a checkpoint entry") };
    assert_eq!(last_checkpoint(&dir).unwrap(), Some(expected));
}

/// The open segment's torn last line is left out, as a whole read leaves it out, and a listed segment
/// the read reaches that differs from its listing is refused.
#[test]
fn each_segment_read_is_checked_as_a_whole_read_checks_it() {
    let dir = scratch_dir("last-checkpoint-torn");
    write(&dir, 2, &[cmd(0), checkpoint(0), cmd(1), cmd(2)]);
    append(&dir.join(SEG_2), &line(&checkpoint(2))[..10]);
    let LogEntry::Checkpoint(expected) = checkpoint(0) else { unreachable!("a checkpoint entry") };
    assert_eq!(last_checkpoint(&dir).unwrap(), Some(expected), "the torn line is left out");

    let dir = scratch_dir("last-checkpoint-differs");
    write(&dir, 2, &[cmd(0), checkpoint(0), cmd(1), cmd(2)]);
    append(&dir.join(SEG_0), b"\n");
    let error = last_checkpoint(&dir).expect_err("a segment that differs from its listing");
    assert!(format!("{error:#}").contains(SEG_0), "{error:#}");
}

/// The manifest is read from its end, a window at a time: a listing far from its end that no longer
/// reads, which a whole read refuses, leaves the newest checkpoint's read as it was, and a checkpoint
/// only the first segment holds is found by growing the window back to the manifest's start.
#[test]
fn the_manifest_is_read_back_from_its_end_only_as_far_as_the_checkpoint() {
    let commands: u64 = 1_000;
    let dir = scratch_dir("last-checkpoint-manifest-end");
    let mut entries: Vec<_> = (0..commands).map(cmd).collect();
    entries.insert(entries.len() - 1, checkpoint(commands - 2));
    write(&dir, 1, &entries);
    let manifest = fs::read(dir.join(MANIFEST)).unwrap();
    assert!(manifest.len() > 64 * 1024, "the manifest outgrows the first window");
    let first = manifest.iter().position(|b| *b == b'\n').unwrap();
    fs::write(dir.join(MANIFEST), [b"not a listing".as_slice(), &manifest[first..]].concat()).unwrap();
    assert!(SegmentedLog::open(&dir, |_| Ok(())).is_err(), "a whole read refuses the manifest's first line");
    let LogEntry::Checkpoint(expected) = checkpoint(commands - 2) else { unreachable!("a checkpoint entry") };
    assert_eq!(last_checkpoint(&dir).unwrap(), Some(expected));

    let dir = scratch_dir("last-checkpoint-first-segment");
    let mut entries: Vec<_> = (0..commands).map(cmd).collect();
    entries.insert(1, checkpoint(0));
    write(&dir, 1, &entries);
    let LogEntry::Checkpoint(expected) = checkpoint(0) else { unreachable!("a checkpoint entry") };
    assert_eq!(last_checkpoint(&dir).unwrap(), Some(expected));
}

/// A segment after the one the writer appends to, which a whole read refuses, is refused: the log is
/// not taken as ending where the manifest's end says, with later commands left on disk.
#[test]
fn a_segment_after_the_one_the_writer_appends_to_is_refused() {
    let dir = scratch_dir("last-checkpoint-later-segment");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), checkpoint(2), cmd(3), cmd(4)]);
    let manifest = fs::read(dir.join(MANIFEST)).unwrap();
    let first = manifest.iter().position(|b| *b == b'\n').unwrap() + 1;
    fs::write(dir.join(MANIFEST), &manifest[..first]).unwrap();
    assert!(SegmentedLog::open(&dir, |_| Ok(())).is_err(), "a whole read refuses the unlisted segment");
    let error = last_checkpoint(&dir).expect_err("a later segment is left on disk");
    assert!(format!("{error:#}").contains("follows the one the writer appends to"), "{error:#}");
}

/// A newest listing whose last seq has no next is refused, not followed into an overflow.
#[test]
fn a_newest_listing_at_the_last_seq_is_refused() {
    let dir = scratch_dir("last-checkpoint-overflow");
    write(&dir, 2, &[cmd(0), cmd(1), checkpoint(1), cmd(2)]);
    let mut listed: SegmentInfo = serde_json::from_slice(fs::read(dir.join(MANIFEST)).unwrap().trim_ascii()).unwrap();
    listed.last_seq = u64::MAX;
    fs::write(dir.join(MANIFEST), [serde_json::to_vec(&listed).unwrap(), b"\n".to_vec()].concat()).unwrap();
    let error = last_checkpoint(&dir).expect_err("no segment follows the last seq");
    assert!(format!("{error:#}").contains("overflows"), "{error:#}");
}

/// A manifest's last listing a crash cut short, before the writer linked the next segment, is left
/// out, as a whole read leaves it out: the segment it was listing is read as the one not listed yet.
#[test]
fn a_listing_a_crash_cut_short_is_left_out() {
    let dir = scratch_dir("last-checkpoint-torn-listing");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), checkpoint(2), cmd(3)]);
    append(&dir.join(MANIFEST), br#"{"file":"0000000000000"#);
    let mut whole = None;
    SegmentedLog::open(&dir, |entry| {
        if let LogEntry::Checkpoint(checkpoint) = entry {
            whole = Some(checkpoint);
        }
        Ok(())
    })
    .unwrap();
    assert!(whole.is_some(), "a whole read finds the checkpoint");
    assert_eq!(last_checkpoint(&dir).unwrap(), whole);
}
