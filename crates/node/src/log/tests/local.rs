//! A log whose oldest listed segments are not on this host, kept only in the public archive: reads from the
//! first segment here on open it, and a read that reaches before it is refused naming the archive.

use std::num::NonZeroUsize;
use std::path::PathBuf;

use super::*;

/// A log of `commands` commands in segments of two, from 0, whose first (listed and completed) is then removed, and the manifest
/// its writer published.
fn archived(name: &str, commands: u64) -> (PathBuf, watch::Receiver<Manifest>) {
    let dir = scratch_dir(name);
    let (manifest, published) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, manifest).unwrap();
    for seq in 0..commands {
        writer.append(&cmd(seq)).unwrap();
    }
    writer.flush().unwrap();
    fs::remove_file(dir.join(SEG_0)).unwrap();
    (dir, published)
}

/// Whether `error` is a read of a segment the archive holds.
fn not_local(error: &anyhow::Error) -> Option<&NotLocal> {
    error.downcast_ref::<NotLocal>()
}

/// The segments from the one holding a read's start open and read, and nothing before them is needed.
#[test]
fn a_log_without_its_oldest_segments_opens_from_the_first_segment_here() {
    let (dir, published) = archived("local-open-from", 6);
    let seqs = |read: &[LogEntry]| read.iter().filter_map(command_seq).collect::<Vec<_>>();
    let mut read = Vec::new();
    SegmentedLog::open_from(&dir, Offset::start_of(3), |entry| {
        read.push(entry);
        Ok(())
    })
    .unwrap();
    assert_eq!(seqs(&read), [3, 4, 5], "the entries from the offset on");
    let mut read = Vec::new();
    SegmentedLog::open_ahead_from(&dir, Offset::start_of(2), |entry| {
        read.push(entry);
        Ok(())
    })
    .unwrap();
    assert_eq!(seqs(&read), [2, 3, 4, 5]);
    let mut read = Vec::new();
    SegmentedLog::read_span_ahead(&dir, 3, |entry| {
        read.push(entry);
        Ok(())
    })
    .unwrap();
    assert_eq!(seqs(&read), [2, 3, 4, 5], "a span reads from the segment holding its command");
    let mut reader = LogReader::open(&dir, Offset::start_of(2), published.clone()).unwrap();
    assert_eq!(reader.next_entry().unwrap().map(|(offset, _)| offset.seq), Some(2));
}

/// A read from an offset hands on every entry after the first at or after it, one that names an earlier offset
/// included, for the reader's own checks to refuse, as a whole read hands it on.
#[test]
fn a_read_from_an_offset_hands_on_an_entry_after_it_that_goes_back() {
    let dir = scratch_dir("local-open-from-back");
    let (manifest, _) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, manifest).unwrap();
    for seq in 0..5 {
        writer.append(&cmd(seq)).unwrap();
    }
    writer.flush().unwrap();
    let line = serde_json::to_string(&cmd(1)).unwrap() + "\n";
    fs::OpenOptions::new().append(true).open(dir.join(SEG_4)).unwrap().write_all(line.as_bytes()).unwrap();
    let seqs = |read: &[LogEntry]| read.iter().filter_map(command_seq).collect::<Vec<_>>();
    let mut read = Vec::new();
    SegmentedLog::open_from(&dir, Offset::start_of(4), |entry| {
        read.push(entry);
        Ok(())
    })
    .unwrap();
    assert_eq!(seqs(&read), [4, 1], "the entry that goes back is handed on");
    let mut read = Vec::new();
    SegmentedLog::open_ahead_from(&dir, Offset::start_of(3), |entry| {
        read.push(entry);
        Ok(())
    })
    .unwrap();
    assert_eq!(seqs(&read), [3, 4, 1], "and read ahead");
}

/// A listing of a segment a read skips that claims seqs a later segment holds is refused at the log's open, so no
/// lookup by seq finds it for a seq the read checked.
#[test]
fn a_skipped_listing_that_claims_a_later_seq_is_refused() {
    let (dir, _) = archived("local-skipped-claims", 6);
    let mut listed = Manifest::load(&dir).unwrap().segments.to_vec();
    listed[0].last_seq = 3;
    let text: String = listed.iter().map(|info| serde_json::to_string(info).unwrap() + "\n").collect();
    fs::write(dir.join(MANIFEST), text).unwrap();
    let refused = SegmentedLog::open_from(&dir, Offset::start_of(3), |_| Ok(())).err().expect("the listing is taken");
    assert!(format!("{refused:#}").contains("not from seq Some(4)"), "{refused:#}");
}

/// A whole read, a read from before the first segment here, the scan and the reader refuse a log whose oldest
/// segments are absent, each with `NotLocal`, which names the archive.
#[test]
fn a_read_before_the_first_segment_here_is_refused_naming_the_archive() {
    let (dir, published) = archived("local-refused", 6);
    let refused = [
        ("open", SegmentedLog::open(&dir, |_| Ok(())).err()),
        ("open_ahead", SegmentedLog::open_ahead(&dir, |_| Ok(())).err()),
        ("open_from", SegmentedLog::open_from(&dir, Offset::start_of(1), |_| Ok(())).err()),
        ("open_ahead_from", SegmentedLog::open_ahead_from(&dir, Offset::start_of(0), |_| Ok(())).err()),
        ("read_span_ahead", SegmentedLog::read_span_ahead(&dir, 1, |_| Ok(())).err()),
        ("uncommanded", uncommanded(&dir, NonZeroUsize::MIN, Some).err()),
        ("read_from", read_from_seq(&dir, 1).err()),
        (
            "reader",
            LogReader::open(&dir, Offset::start_of(1), published.clone())
                .and_then(|mut reader| reader.next_entry())
                .err(),
        ),
    ];
    for (read, error) in refused {
        let error = error.unwrap_or_else(|| panic!("{read} read the absent segment"));
        let not_local = not_local(&error).unwrap_or_else(|| panic!("{read}: {error:#}"));
        assert!(not_local.to_string().contains("public archive"), "{read}: {not_local}");
    }
}

/// The log's entry at the segment before the first here is the archive's, though its listing is in the manifest.
#[test]
fn a_reader_naming_an_absent_segment_is_refused_and_one_at_the_first_here_is_not() {
    let (dir, _) = archived("local-reader", 6);
    read_from_seq(&dir, 2).unwrap();
    read_from_seq(&dir, 5).unwrap();
    let error = read_from_seq(&dir, 0).expect_err("seq 0 is the archive's");
    assert_eq!(not_local(&error).map(|held| (held.seq, held.first_local)), Some((0, Some(2))));
}

/// A segment missing after the first one here is a damaged log, not an archived one: the manifest's listings
/// must pair with the files by name from the first here on.
#[test]
fn a_segment_missing_after_the_first_here_is_refused() {
    let (dir, _) = archived("local-gap", 8);
    fs::remove_file(dir.join(SEG_4)).unwrap();
    for error in [
        SegmentedLog::open_from(&dir, Offset::start_of(6), |_| Ok(())).err().expect("a gap"),
        SegmentedLog::read_span_ahead(&dir, 6, |_| Ok(())).expect_err("a gap"),
    ] {
        assert!(not_local(&error).is_none(), "{error:#}");
        assert!(format!("{error:#}").contains("a listed segment after the first on this host is missing"), "{error:#}");
    }
}

/// Reads `log::read_from` from seq `from` to the end.
fn read_from_seq(dir: &Path, from: u64) -> anyhow::Result<()> {
    super::read_from(dir, from, |_| Ok(()))
}
