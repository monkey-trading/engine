//! Log reader tests: offsets, following the writer, checks, and release.

use super::*;

use crate::test_support::published_manifest;

#[test]
fn the_reader_starts_at_any_offset_across_segments() {
    let dir = scratch_dir("reader-offsets");
    write(&dir, 2, &[cmd(0), cmd(1), withdrawal(1), checkpoint(1), cmd(2), cmd(3)]);
    let all = [(0, 0), (1, 0), (1, 1), (1, 2), (2, 0), (3, 0)];
    assert_eq!(read_from(&dir, 0, 0), all);
    assert_eq!(read_from(&dir, 1, 1), all[2..]);
    assert_eq!(read_from(&dir, 2, 0), all[4..]);
    assert_eq!(read_from(&dir, 1, 7), all[4..]);
    assert!(read_from(&dir, 9, 0).is_empty());
}

#[test]
fn the_reader_leaves_uncommitted_lines_for_a_later_writer_flush() {
    let dir = scratch_dir("reader-partial");
    let (manifest_tx, manifest) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(4, manifest_tx).unwrap();
    writer.append(&cmd(0)).unwrap();
    writer.flush().unwrap();
    let path = dir.join(SEG_0);
    let prefix_len = fs::metadata(&path).unwrap().len();
    let mut reader = LogReader::open(&dir, Offset::start_of(0), manifest).unwrap();
    assert_eq!(reader.next_entry().unwrap().map(|(o, _)| o), Some(Offset::start_of(0)));
    assert!(reader.next_entry().unwrap().is_none());
    let bytes = line(&cmd(1));
    append(&path, &bytes[..10]);
    assert!(reader.next_entry().unwrap().is_none());
    append(&path, &bytes[10..]);
    assert!(reader.next_entry().unwrap().is_none(), "a complete host line is still uncommitted");
    fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(prefix_len).unwrap();
    writer.append(&cmd(1)).unwrap();
    writer.flush().unwrap();
    assert_eq!(reader.next_entry().unwrap(), Some((Offset::start_of(1), cmd(1))));
}

#[test]
fn the_reader_follows_the_writer_into_a_new_segment() {
    let dir = scratch_dir("reader-follow");
    let (manifest_tx, manifest) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, manifest_tx).unwrap();
    let mut reader = LogReader::open(&dir, Offset::start_of(0), manifest).unwrap();
    assert!(reader.next_entry().unwrap().is_none());

    writer.append(&cmd(0)).unwrap();
    writer.flush().unwrap();
    assert_eq!(reader.next_entry().unwrap(), Some((Offset::start_of(0), cmd(0))));
    assert!(reader.next_entry().unwrap().is_none());
    for seq in 1..=4 {
        writer.append(&cmd(seq)).unwrap();
    }
    writer.flush().unwrap();
    let offsets: Vec<_> = std::iter::from_fn(|| reader.next_entry().unwrap()).map(|(o, _)| o.seq).collect();
    assert_eq!(offsets, [1, 2, 3, 4]);
}

#[test]
fn the_reader_refuses_a_completed_segment_that_differs_from_its_listing() {
    let dir = scratch_dir("reader-tampered");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    let manifest = published_manifest(&dir);
    let path = dir.join(SEG_0);
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replacen("\"ts\":0,", "\"ts\":9,", 1)).unwrap();
    let mut reader = LogReader::open(&dir, Offset::start_of(0), manifest).unwrap();
    let err = reader.next_entry().err().unwrap();
    assert!(format!("{err:#}").contains("does not match its listing"), "{err:#}");
}

#[test]
fn the_reader_refuses_a_completed_segment_that_ends_mid_line() {
    let dir = scratch_dir("reader-torn");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    let manifest = published_manifest(&dir);
    let path = dir.join(SEG_0);
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
    let mut reader = LogReader::open(&dir, Offset::start_of(0), manifest).unwrap();
    assert!(reader.next_entry().is_err(), "no completed bytes before their exact commitment matches");
}

#[test]
fn the_reader_refuses_a_missing_required_segment_or_start_of_the_log() {
    let dir = scratch_dir("reader-gap");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3), cmd(4), cmd(5)]);
    let manifest = published_manifest(&dir);
    fs::remove_file(dir.join(SEG_2)).unwrap();
    for from in [Offset::start_of(0), Offset::start_of(2)] {
        let mut reader = LogReader::open(&dir, from, manifest.clone()).unwrap();
        let err = std::iter::from_fn(|| reader.next_entry().transpose()).find_map(Result::err).unwrap();
        assert!(format!("{err:#}").contains(SEG_2), "{err:#}");
    }
    fs::remove_file(dir.join(SEG_0)).unwrap();
    let mut reader = LogReader::open(&dir, Offset::start_of(0), manifest).unwrap();
    let err = reader.next_entry().err().unwrap();
    assert!(format!("{err:#}").contains(SEG_0), "{err:#}");
}

#[test]
fn the_reader_refuses_a_host_rewrite_that_removes_a_segments_first_command() {
    let dir = scratch_dir("reader-headless");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    let manifest = published_manifest(&dir);
    fs::write(dir.join(SEG_2), [line(&checkpoint(1)), line(&cmd(2))].concat()).unwrap();
    let mut reader = LogReader::open(&dir, Offset::start_of(0), manifest).unwrap();
    let err = std::iter::from_fn(|| reader.next_entry().transpose()).find_map(Result::err).unwrap();
    assert!(format!("{err:#}").contains("flushed prefix"), "{err:#}");
}

#[test]
fn a_released_reader_resumes_where_it_stopped_and_checks_its_segment_again() {
    let dir = scratch_dir("reader-release");
    write(&dir, 3, &[cmd(0), cmd(1), withdrawal(1), cmd(2), cmd(3)]);
    let mut reader = LogReader::open(&dir, Offset::start_of(0), published_manifest(&dir)).unwrap();
    let mut seen = vec![reader.next_entry().unwrap().unwrap().0, reader.next_entry().unwrap().unwrap().0];
    reader.release();
    seen.extend(std::iter::from_fn(|| reader.next_entry().unwrap()).map(|(o, _)| o));
    assert_eq!(seen, read_from(&dir, 0, 0).into_iter().map(|(seq, index)| Offset { seq, index }).collect::<Vec<_>>());

    let mut reader = LogReader::open(&dir, Offset::start_of(0), published_manifest(&dir)).unwrap();
    reader.next_entry().unwrap();
    reader.release();
    let text = fs::read_to_string(dir.join(SEG_0)).unwrap();
    fs::write(dir.join(SEG_0), text.replacen("\"ts\":10,", "\"ts\":19,", 1)).unwrap();
    let err = reader.next_entry().err().unwrap();
    assert!(format!("{err:#}").contains("does not match its listing"), "{err:#}");
}

#[test]
fn a_released_reader_hashes_its_segment_once_and_then_checks_only_the_chunks_it_rereads() {
    let dir = scratch_dir("reader-release-once");
    write(&dir, 3, &[large(0), large(1), large(2), large(3)]);
    let path = dir.join(SEG_0);
    let mut reader = LogReader::open(&dir, Offset::start_of(0), published_manifest(&dir)).unwrap();
    assert_eq!(reader.next_entry().unwrap().map(|(o, _)| o.seq), Some(0));
    reader.release();
    // Hashing the whole segment again would now fail.
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replacen("\"ts\":0,", "\"ts\":9,", 1)).unwrap();
    assert_eq!(reader.next_entry().unwrap().map(|(o, _)| o.seq), Some(1));
    reader.release();
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replacen("\"ts\":20,", "\"ts\":29,", 1)).unwrap();
    let err = reader.next_entry().err().unwrap();
    assert!(format!("{err:#}").contains("does not match its listing"), "{err:#}");
}

#[test]
fn a_growing_active_prefix_is_hashed_from_where_its_last_check_ended() {
    let dir = scratch_dir("reader-prefix-growth");
    let (manifest_tx, manifest) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(8, manifest_tx).unwrap();
    writer.append(&large(0)).unwrap();
    writer.flush().unwrap();
    let path = dir.join(SEG_0);
    let mut reader = LogReader::open(&dir, Offset::start_of(0), manifest).unwrap();
    assert_eq!(seqs(&mut reader), [0]);
    // Hashing the grown prefix from its first byte would now fail.
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replacen("\"ts\":0,", "\"ts\":9,", 1)).unwrap();
    writer.append(&large(1)).unwrap();
    writer.flush().unwrap();
    assert_eq!(seqs(&mut reader), [1]);
    writer.append(&large(2)).unwrap();
    writer.flush().unwrap();
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replacen("\"ts\":20,", "\"ts\":29,", 1)).unwrap();
    let err = reader.next_entry().err().unwrap();
    assert!(format!("{err:#}").contains("flushed prefix"), "{err:#}");
}

#[test]
fn runtime_readers_ignore_unreferenced_files_and_strict_full_audit_rejects_them() {
    let dir = scratch_dir("reader-planted");
    write(&dir, 4, &[cmd(0), cmd(1), cmd(2), cmd(3), cmd(4)]);
    let manifest = published_manifest(&dir);
    fs::write(dir.join(SEG_2), [line(&cmd(2)), line(&cmd(3))].concat()).unwrap();
    let mut reader = LogReader::open(&dir, Offset::start_of(2), manifest).unwrap();
    let actual = std::iter::from_fn(|| reader.next_entry().unwrap()).map(|(offset, _)| offset.seq).collect::<Vec<_>>();
    assert_eq!(actual, [2, 3, 4]);
    assert!(SegmentedLog::open(&dir, |_| Ok(())).is_err(), "full audit still diagnoses the orphan");
}

#[test]
fn the_reader_yields_only_what_the_writer_has_flushed() {
    let dir = scratch_dir("reader-flushed");
    let (manifest_tx, manifest) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, manifest_tx).unwrap();
    writer.append(&cmd(0)).unwrap();
    writer.flush().unwrap();
    let mut follower = LogReader::open(&dir, Offset::start_of(0), manifest.clone()).unwrap();
    assert_eq!(seqs(&mut follower), [0]);

    // One batch, all on disk before it is flushed: a line longer than the write buffer goes
    // straight to the file, and starting a segment completes the one before it.
    assert!(line(&large(1)).len() > 8 * 1024);
    for entry in [large(1), cmd(2), large(3)] {
        writer.append(&entry).unwrap();
    }
    assert_eq!(log_entries(&dir), [cmd(0), large(1), cmd(2), large(3)]);
    let mut fresh = LogReader::open(&dir, Offset::start_of(0), manifest.clone()).unwrap();
    assert_eq!(seqs(&mut fresh), [0]);
    assert!(seqs(&mut follower).is_empty());

    writer.flush().unwrap();
    assert_eq!(seqs(&mut follower), [1, 2, 3]);
    assert_eq!(seqs(&mut fresh), [1, 2, 3]);
}

// ── Helpers ──

/// Every `(seq, index)` a reader opened at `seq:index` yields.
fn read_from(dir: &Path, seq: u64, index: u32) -> Vec<(u64, u32)> {
    let mut reader = LogReader::open(dir, Offset { seq, index }, published_manifest(dir)).unwrap();
    std::iter::from_fn(|| reader.next_entry().unwrap()).map(|(o, _)| (o.seq, o.index)).collect()
}

/// The seqs of every entry `reader` yields now.
fn seqs(reader: &mut LogReader) -> Vec<u64> {
    std::iter::from_fn(|| reader.next_entry().unwrap()).map(|(o, _)| o.seq).collect()
}

/// A command whose line is longer than the writer's buffer.
fn large(seq: u64) -> LogEntry {
    let deposit = Event::Deposited { account: AccountId([1; 20]), amount: Fixed::from_int(5) };
    LogEntry::Command(CommandEntry {
        sequenced: Sequenced { seq, ts: seq as i64 * 10, cmd: Command::Tick },
        events: Some(vec![deposit; 200]),
    })
}
