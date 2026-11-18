//! A last line a crash cut short: left out of the open segment and dropped before the next append,
//! and refused in a completed segment.

use super::*;

/// The open segment's last line with no break, after whole lines or as its only line, is left out of
/// a read, and the writer drops it, so the entry appended next starts on its own line.
#[test]
fn a_line_a_crash_cut_short_is_left_out_and_dropped_before_the_next_append() {
    for whole in [1, 0] {
        let dir = scratch_dir(&format!("torn-open-segment-{whole}"));
        write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
        // The open segment keeps `whole` whole lines, then the next command's line cut short.
        let cut = 2 + whole;
        let kept: Vec<u8> = (2..cut).flat_map(|seq| line(&cmd(seq))).collect();
        let next = line(&cmd(cut));
        fs::write(dir.join(SEG_2), [kept.as_slice(), &next[..next.len() / 2]].concat()).unwrap();
        let mut seen = Vec::new();
        let log = SegmentedLog::open(&dir, |entry| {
            seen.push(entry);
            Ok(())
        })
        .unwrap();
        assert_eq!((seen, log.last_seq()), ((0..cut).map(cmd).collect::<Vec<_>>(), Some(cut - 1)), "{whole} whole");
        write(&dir, 2, &[cmd(cut)]);
        assert_eq!(fs::read(dir.join(SEG_2)).unwrap(), [kept, next].concat(), "{whole} whole");
        assert_eq!(log_entries(&dir), (0..=cut).map(cmd).collect::<Vec<_>>(), "{whole} whole");
    }
}

/// The writer drops only the bytes its read left out: a segment that changed after the read refuses
/// the append, as any change does.
#[test]
fn a_torn_segment_that_changed_after_its_read_refuses_the_append() {
    let dir = scratch_dir("torn-then-changed");
    write(&dir, 2, &[cmd(0)]);
    append(&dir.join(SEG_0), br#"{"entry":"#);
    let log = SegmentedLog::open(&dir, |_| Ok(())).unwrap();
    append(&dir.join(SEG_0), br#""command"}"#);
    let err = log.into_writer(2, watch::channel(Manifest::default()).0).err().unwrap();
    assert!(format!("{err:#}").contains("length changed before append"), "{err:#}");
}

/// A completed segment was synced whole before it was listed, so one that ends mid-line is refused,
/// even under a listing of exactly its bytes.
#[test]
fn a_completed_segment_that_ends_mid_line_is_refused() {
    let dir = scratch_dir("torn-completed-segment");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    let bytes = [line(&cmd(0)), line(&cmd(1))[..10].to_vec()].concat();
    fs::write(dir.join(SEG_0), &bytes).unwrap();
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let listing = SegmentInfo { file: SEG_0.into(), first_seq: 0, last_seq: 1, bytes: bytes.len() as u64, sha256 };
    fs::write(dir.join(MANIFEST), [serde_json::to_vec(&listing).unwrap(), b"\n".to_vec()].concat()).unwrap();
    let err = SegmentedLog::open(&dir, |_| Ok(())).err().unwrap();
    assert!(format!("{err:#}").contains("ends mid-line"), "{err:#}");
}
