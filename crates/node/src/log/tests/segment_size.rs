//! A segment's size: the writer starts the next segment before a command whose line would take the open one past
//! `MAX_SEGMENT_BYTES`, however few commands it holds, so a member fetches every segment whole; a line larger than
//! the bound takes a segment of its own; and a log reopened on a nearly full segment goes on by the same rule.

use super::*;
use crate::log::segments::MAX_SEGMENT_BYTES;

/// A command at `seq` whose line is about `bytes` long.
fn sized(seq: u64, bytes: usize) -> LogEntry {
    let cmd = Command::HouseCapital { amount: Fixed::ZERO, chain_ref: "x".repeat(bytes) };
    LogEntry::Command(CommandEntry { seal: None, sequenced: Sequenced { seq, ts: seq as i64 * 10, cmd }, events: None })
}

#[test]
fn a_segment_ends_before_a_command_that_would_pass_the_size_a_member_fetches() {
    let dir = scratch_dir("segment-size");
    let line = (MAX_SEGMENT_BYTES / 8) as usize;
    // Sixteen lines of an eighth of the bound, then one larger than the bound, then two small, with room for a
    // thousand commands a segment.
    let mut entries: Vec<LogEntry> = (0..16).map(|seq| sized(seq, line)).collect();
    entries.push(sized(16, MAX_SEGMENT_BYTES as usize + 1));
    entries.extend([cmd(17), cmd(18)]);
    write(&dir, 1_000, &entries[..10]);
    // Reopened on a segment holding three of them, the writer counts their bytes.
    write(&dir, 1_000, &entries[10..]);

    let manifest = Manifest::load(&dir).unwrap();
    let firsts: Vec<_> = manifest.segments.iter().map(|segment| segment.first_seq).collect();
    // Seven lines and a bit fit; the eighth would pass the bound.
    assert_eq!(firsts, [0, 7, 14, 16], "{:?}", manifest.segments);
    for listed in manifest.segments.iter().filter(|segment| segment.first_seq != 16) {
        assert!(listed.bytes <= MAX_SEGMENT_BYTES, "{listed:?}");
    }
    const _: () = assert!(MAX_SEGMENT_BYTES < crate::standby::MAX_ARCHIVE_BYTES);
    let oversize = manifest.segments.iter().find(|segment| segment.first_seq == 16).expect("the large line's");
    assert_eq!((oversize.first_seq, oversize.last_seq), (16, 16), "a line past the bound takes a segment alone");
    assert_eq!(log_entries(&dir), entries);
}
