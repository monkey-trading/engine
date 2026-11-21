//! A read of the log from the segment holding a given command (`SegmentedLog::read_span_ahead`): no
//! segment before it is read, and each it reads is checked as a whole read checks it.

use super::*;

/// The span hands over every entry from the first of the segment holding the command, as a whole read
/// gives them, reads no segment before it, and ends without error where `visit` stops it.
#[test]
fn a_span_reads_from_the_segment_holding_its_command() {
    let dir = scratch_dir("span-from");
    let entries = [cmd(0), cmd(1), cmd(2), checkpoint(2), cmd(3), cmd(4)];
    write(&dir, 2, &entries);
    fs::write(dir.join(SEG_0), b"not a log entry\n").unwrap();
    assert!(SegmentedLog::open(&dir, |_| Ok(())).is_err(), "a whole read refuses the first segment");
    let mut read = Vec::new();
    SegmentedLog::read_span_ahead(&dir, 3, |entry| {
        read.push(entry);
        Ok(())
    })
    .unwrap();
    assert_eq!(read, entries[2..]);
    let mut taken = 0;
    SegmentedLog::read_span_ahead(&dir, 2, |_| {
        taken += 1;
        if taken == 2 { Err(Stop.into()) } else { Ok(()) }
    })
    .unwrap();
    assert_eq!(taken, 2, "the read ends where the visitor stops it");
}

/// An empty unlisted last segment named for any seq but the one after the log's last, which a whole read
/// refuses, is refused by a span too; one named for the next seq is the writer's new segment.
#[test]
fn a_span_refuses_an_empty_last_segment_a_whole_read_refuses() {
    let dir = scratch_dir("span-empty-last");
    // As if the writer listed segment 2 and stopped as it started the next.
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3), cmd(4)]);
    fs::write(dir.join(SEG_4), "").unwrap();
    SegmentedLog::read_span_ahead(&dir, 2, |_| Ok(())).unwrap();
    fs::rename(dir.join(SEG_4), dir.join("00000000000000000006.jsonl")).unwrap();
    let whole = SegmentedLog::open(&dir, |_| Ok(())).err().expect("a whole read refuses it");
    let span = SegmentedLog::read_span_ahead(&dir, 2, |_| Ok(())).expect_err("a span refuses it");
    for error in [whole, span] {
        assert!(format!("{error:#}").contains("not named for the next seq 4"), "{error:#}");
    }
}
