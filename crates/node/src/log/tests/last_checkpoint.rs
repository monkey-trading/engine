//! The log's last checkpoint, read from its newest segment back: the one a whole read finds, reading
//! no segment before the newest that holds a checkpoint, each it reads checked as a whole read checks it.

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

/// The open segment's torn last line is left out, as a whole read leaves it out; a listed segment the
/// read reaches that differs from its listing is refused, as is an unlisted one before the last.
#[test]
fn each_segment_read_is_checked_as_a_whole_read_checks_it() {
    let dir = scratch_dir("last-checkpoint-torn");
    write(&dir, 2, &[cmd(0), checkpoint(0), cmd(1), cmd(2)]);
    append(&dir.join(SEG_2), &line(&checkpoint(2))[..10]);
    let LogEntry::Checkpoint(expected) = checkpoint(0) else { unreachable!("a checkpoint entry") };
    assert_eq!(last_checkpoint(&dir).unwrap(), Some(expected), "the torn line is left out");

    let dir = scratch_dir("last-checkpoint-unlisted");
    write(&dir, 2, &[cmd(0), checkpoint(0), cmd(1), cmd(2)]);
    append(&dir.join(SEG_0), b"\n");
    let error = last_checkpoint(&dir).expect_err("a segment that differs from its listing");
    assert!(format!("{error:#}").contains(SEG_0), "{error:#}");
    fs::remove_file(dir.join(MANIFEST)).unwrap();
    let error = last_checkpoint(&dir).expect_err("an unlisted segment before the last");
    assert!(format!("{error:#}").contains("is not in"), "{error:#}");
}
