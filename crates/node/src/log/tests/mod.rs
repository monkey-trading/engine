use super::*;
use std::fs;
use std::io::Write as _;

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::types::AccountId;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::alerts::{Severity, capture};
use crate::test_support::{every_event, file_entries, log_entries, scratch_dir};

mod durability;
mod reader;

const SEG_0: &str = "00000000000000000000.jsonl";
const SEG_2: &str = "00000000000000000002.jsonl";
const SEG_4: &str = "00000000000000000004.jsonl";

#[test]
fn offsets_read_and_write_as_seq_colon_index_and_order_by_seq_then_index() {
    let offset: Offset = "12:3".parse().unwrap();
    assert_eq!(offset, Offset { seq: 12, index: 3 });
    assert_eq!(offset.to_string(), "12:3");
    assert_eq!(serde_json::to_string(&offset).unwrap(), "\"12:3\"");
    assert_eq!(serde_json::from_str::<Offset>("\"12:3\"").unwrap(), offset);
    for bad in ["12", "12:", ":3", "a:3", "12:-1", "12:3:4"] {
        assert!(bad.parse::<Offset>().is_err(), "{bad}");
    }
    assert!(Offset { seq: 1, index: 9 } < Offset { seq: 2, index: 0 });
    assert!(Offset { seq: 2, index: 0 } < Offset { seq: 2, index: 1 });
    assert_eq!(offset.next(), Offset { seq: 12, index: 4 });
}

#[test]
fn a_command_entry_from_before_events_were_logged_still_parses() {
    let line = r#"{"entry":"command","seq":0,"ts":5,"cmd":{"type":"tick"}}"#;
    let entry: LogEntry = serde_json::from_str(line).unwrap();
    let sequenced = Sequenced { seq: 0, ts: 5, cmd: Command::Tick };
    assert_eq!(entry, LogEntry::Command(CommandEntry { sequenced, events: None }));
    assert_eq!(serde_json::to_string(&entry).unwrap(), line);
}

#[test]
fn a_command_entry_carries_its_events_through_the_file_format() {
    let events = vec![Event::Deposited { account: AccountId([1; 20]), amount: Fixed::from_int(5) }];
    let entry = LogEntry::Command(CommandEntry {
        sequenced: Sequenced { seq: 1, ts: 5, cmd: Command::Tick },
        events: Some(events),
    });
    let line = serde_json::to_string(&entry).unwrap();
    assert!(line.contains(r#""events":[{"type":"deposited""#), "{line}");
    assert_eq!(serde_json::from_str::<LogEntry>(&line).unwrap(), entry);
}

#[test]
fn every_event_and_rejection_reason_survives_the_file_format() {
    let entry = LogEntry::Command(CommandEntry {
        sequenced: Sequenced { seq: 1, ts: 5, cmd: Command::Tick },
        events: Some(every_event()),
    });
    let line = serde_json::to_string(&entry).unwrap();
    assert_eq!(serde_json::from_str::<LogEntry>(&line).unwrap(), entry, "{line}");
}

#[test]
fn offsets_number_each_seqs_entries_from_its_command() {
    let entries = [cmd(0), cmd(1), withdrawal(1), checkpoint(1), cmd(2)];
    let offsets: Vec<_> = entries
        .iter()
        .scan(None, |prev, entry| {
            *prev = Some(offset_of(*prev, entry).unwrap());
            *prev
        })
        .map(|o| (o.seq, o.index))
        .collect();
    assert_eq!(offsets, [(0, 0), (1, 0), (1, 1), (1, 2), (2, 0)]);
    assert!(offset_of(None, &checkpoint(0)).is_err());
    assert!(offset_of(None, &withdrawal(1)).is_err());
}

#[test]
fn sync_writes_buffered_entries_to_the_segment() {
    let dir = scratch_dir("sync");
    let manifest = watch::channel(Manifest::default()).0;
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(100, manifest).unwrap();
    writer.append(&cmd(0)).unwrap();
    assert!(file_entries(&dir.join(SEG_0)).is_empty(), "appends stay buffered");
    writer.sync().unwrap();
    assert_eq!(file_entries(&dir.join(SEG_0)), [cmd(0)]);
}

#[test]
fn a_missing_log_file_and_a_corrupt_line_are_errors() {
    let path = scratch_dir("corrupt").join("log.jsonl");
    let err = read_file(&path, |_| Ok(())).unwrap_err();
    assert!(format!("{err:#}").contains("opening log"), "{err:#}");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "\n{\"entry\":\"command\",\"seq\":0,\"ts\":0,\"cmd\":{\"type\":\"tick\"}}\nnot json\n").unwrap();
    let err = read_file(&path, |_| Ok(())).unwrap_err();
    assert!(format!("{err:#}").contains("log line 3"), "{err:#}");
}

#[test]
fn segments_start_at_a_command_and_the_manifest_lists_completed_ones() {
    let dir = scratch_dir("rotate");
    let entries = [cmd(0), cmd(1), withdrawal(1), checkpoint(1), cmd(2), cmd(3), cmd(4)];
    write(&dir, 2, &entries);

    assert_eq!(files(&dir), [SEG_0, SEG_2, SEG_4, MANIFEST]);
    // Seq 1's withdrawal and checkpoint stay with its command.
    assert_eq!(file_entries(&dir.join(SEG_0)), entries[..4]);
    let manifest = Manifest::load(&dir).unwrap();
    let ranges: Vec<_> = manifest.segments.iter().map(|s| (s.file.as_str(), s.first_seq, s.last_seq)).collect();
    assert_eq!(ranges, [(SEG_0, 0, 1), (SEG_2, 2, 3)]);
    for listed in manifest.segments.iter() {
        let bytes = fs::read(dir.join(&listed.file)).unwrap();
        assert_eq!(listed.bytes, bytes.len() as u64);
        assert_eq!(listed.sha256, hex::encode(Sha256::digest(&bytes)));
    }
    assert_eq!(log_entries(&dir), entries);
}

#[test]
fn a_reopened_log_lists_its_full_last_segment_at_the_next_command() {
    // Also what a crash between syncing a full segment and storing the manifest leaves.
    let dir = scratch_dir("reopen-full");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3)]);
    assert_eq!(Manifest::load(&dir).unwrap().segments.len(), 1);

    write(&dir, 2, &[cmd(4)]);
    let ranges: Vec<_> = Manifest::load(&dir).unwrap().segments.iter().map(|s| (s.first_seq, s.last_seq)).collect();
    assert_eq!(ranges, [(0, 1), (2, 3)]);
    assert_eq!(files(&dir), [SEG_0, SEG_2, SEG_4, MANIFEST]);
    assert_eq!(log_entries(&dir), [cmd(0), cmd(1), cmd(2), cmd(3), cmd(4)]);
}

#[test]
fn a_reopened_log_appends_to_a_last_segment_with_room() {
    let dir = scratch_dir("reopen-room");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    write(&dir, 2, &[cmd(3)]);
    assert_eq!(files(&dir), [SEG_0, SEG_2, MANIFEST]);
    assert_eq!(file_entries(&dir.join(SEG_2)), [cmd(2), cmd(3)]);
}

#[test]
fn a_crash_after_listing_a_segment_but_before_starting_the_next_resumes_cleanly() {
    let dir = scratch_dir("crash-listed");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3), cmd(4)]);
    // As if the writer stopped right after listing segment 2.
    fs::remove_file(dir.join(SEG_4)).unwrap();

    write(&dir, 2, &[cmd(4), cmd(5)]);
    assert_eq!(files(&dir), [SEG_0, SEG_2, SEG_4, MANIFEST]);
    assert_eq!(file_entries(&dir.join(SEG_4)), [cmd(4), cmd(5)]);
    assert_eq!(Manifest::load(&dir).unwrap().segments.len(), 2);
}

#[test]
fn a_crash_after_creating_a_segment_but_before_writing_it_resumes_cleanly() {
    let dir = scratch_dir("crash-empty");
    write(&dir, 2, &[cmd(0), cmd(1)]);
    write(&dir, 2, &[cmd(2)]);
    fs::write(dir.join(SEG_2), "").unwrap();

    write(&dir, 2, &[cmd(2)]);
    assert_eq!(log_entries(&dir), [cmd(0), cmd(1), cmd(2)]);
    // An empty last segment named for any other seq is refused.
    fs::rename(dir.join(SEG_2), dir.join(SEG_4)).unwrap();
    fs::write(dir.join(SEG_4), "").unwrap();
    let err = SegmentedLog::open(&dir, |_| Ok(())).err().unwrap();
    assert!(format!("{err:#}").contains("not named for the next seq 2"), "{err:#}");
}

#[test]
fn opening_refuses_a_segment_that_differs_from_its_manifest_entry() {
    let dir = scratch_dir("tampered");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    let path = dir.join(SEG_0);
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replacen("\"ts\":0,", "\"ts\":9,", 1)).unwrap();
    let err = SegmentedLog::open(&dir, |_| Ok(())).err().unwrap();
    assert!(format!("{err:#}").contains(&format!("does not match its listing in {MANIFEST}")), "{err:#}");

    fs::remove_file(&path).unwrap();
    let err = SegmentedLog::open(&dir, |_| Ok(())).err().unwrap();
    assert!(format!("{err:#}").contains(&format!("does not match its listing in {MANIFEST}")), "{err:#}");
}

#[test]
fn opening_refuses_an_unlisted_segment_that_is_not_the_last() {
    let dir = scratch_dir("unlisted");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    fs::remove_file(dir.join(MANIFEST)).unwrap();
    let err = SegmentedLog::open(&dir, |_| Ok(())).err().unwrap();
    assert!(format!("{err:#}").contains(&format!("is not in {MANIFEST}")), "{err:#}");
}

#[test]
fn opening_refuses_a_segment_named_for_another_seq_or_ending_mid_line() {
    let dir = scratch_dir("misnamed");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    fs::rename(dir.join(SEG_2), dir.join("00000000000000000003.jsonl")).unwrap();
    let err = SegmentedLog::open(&dir, |_| Ok(())).err().unwrap();
    assert!(format!("{err:#}").contains("does not start with the command for seq 3"), "{err:#}");

    let dir = scratch_dir("torn");
    write(&dir, 2, &[cmd(0)]);
    append(&dir.join(SEG_0), b"{\"entry\":");
    let err = SegmentedLog::open(&dir, |_| Ok(())).err().unwrap();
    assert!(format!("{err:#}").contains("ends mid-line"), "{err:#}");
}

#[test]
fn a_log_written_as_one_file_before_segments_continues_as_the_first_segment() {
    let dir = scratch_dir("legacy");
    fs::create_dir_all(&dir).unwrap();
    let legacy = concat!(
        r#"{"entry":"command","seq":0,"ts":0,"cmd":{"type":"tick"}}"#,
        "\n",
        r#"{"entry":"checkpoint","seq":0,"root":"0x00","signature":"0x01","signer":"0x02"}"#,
        "\n",
    );
    fs::write(dir.join(SEG_0), legacy).unwrap();
    write(&dir, 2, &[cmd(1), cmd(2)]);
    let entries = log_entries(&dir);
    assert!(matches!(&entries[0], LogEntry::Command(c) if c.events.is_none()));
    assert_eq!(entries[2..], [cmd(1), cmd(2)]);
    assert_eq!(files(&dir), [SEG_0, SEG_2, MANIFEST]);
}

#[test]
fn opening_refuses_a_log_missing_its_newest_listed_segment() {
    let dir = scratch_dir("missing-newest");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3), cmd(4)]);
    fs::remove_file(dir.join(SEG_4)).unwrap();
    fs::remove_file(dir.join(SEG_2)).unwrap();
    let err = SegmentedLog::open(&dir, |_| Ok(())).err().unwrap();
    assert!(format!("{err:#}").contains(&format!("{MANIFEST} lists 2 segments but 1 are")), "{err:#}");
}

#[test]
fn the_manifest_gets_a_line_per_completed_segment_and_drops_a_torn_last_line() {
    let dir = scratch_dir("manifest-lines");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3), cmd(4)]);
    let text = fs::read_to_string(dir.join(MANIFEST)).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert_eq!(text.as_bytes(), Manifest::load(&dir).unwrap().to_jsonl().unwrap());

    // A crash while listing leaves part of a line; it is dropped and trimmed before the next one.
    append(&dir.join(MANIFEST), b"{\"file\":\"0000");
    assert_eq!(Manifest::load(&dir).unwrap().segments.len(), 2);
    write(&dir, 2, &[cmd(5), cmd(6)]);
    let text = fs::read_to_string(dir.join(MANIFEST)).unwrap();
    assert_eq!(text.lines().count(), 3, "{text}");
    assert_eq!(Manifest::load(&dir).unwrap().segments[2].first_seq, 4);
}

#[test]
fn a_segment_that_cannot_be_started_leaves_the_current_one_growing_until_it_can() {
    let dir = scratch_dir("rotation-retry");
    let manifest = watch::channel(Manifest::default()).0;
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, manifest).unwrap();
    writer.append(&cmd(0)).unwrap();
    writer.append(&cmd(1)).unwrap();
    // The writer keeps its current descriptor while a host path change blocks new files.
    let displaced = dir.with_extension("displaced");
    fs::rename(&dir, &displaced).unwrap();
    fs::write(&dir, b"not a directory").unwrap();
    let alerts = capture(|| {
        writer.append(&cmd(2)).unwrap();
        fs::remove_file(&dir).unwrap();
        fs::rename(&displaced, &dir).unwrap();
        writer.append(&cmd(3)).unwrap();
    });
    writer.flush().unwrap();
    // The failure pages once, and recovering resolves it.
    let [failed, resolved] = alerts.as_slice() else { panic!("{alerts:?}") };
    assert_eq!((failed.key(), failed.severity), ("log_rotation_failed{}".to_owned(), Some(Severity::Warning)));
    assert_eq!((resolved.key(), resolved.severity), (failed.key(), None));

    assert_eq!(file_entries(&dir.join(SEG_0)), [cmd(0), cmd(1), cmd(2)]);
    assert_eq!(file_entries(&dir.join("00000000000000000003.jsonl")), [cmd(3)]);
    let ranges: Vec<_> = Manifest::load(&dir).unwrap().segments.iter().map(|s| (s.first_seq, s.last_seq)).collect();
    assert_eq!(ranges, [(0, 2)]);
}

#[test]
fn opening_hands_on_each_segment_before_it_reads_the_next() {
    let dir = scratch_dir("open-streams");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3), cmd(4)]);
    // Removing the newest segment once the first entry is handed on fails the open only if that
    // segment is read after it.
    let mut seen = Vec::new();
    let err = SegmentedLog::open(&dir, |entry| {
        if seen.is_empty() {
            fs::remove_file(dir.join(SEG_4)).unwrap();
        }
        seen.push(entry);
        Ok(())
    })
    .err()
    .unwrap();
    assert!(format!("{err:#}").contains(&format!("opening {}", dir.join(SEG_4).display())), "{err:#}");
    assert_eq!(seen, [cmd(0), cmd(1), cmd(2), cmd(3)]);
}

#[test]
fn opening_hands_on_nothing_of_a_completed_segment_that_differs_from_its_listing() {
    let dir = scratch_dir("open-checks-first");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2), cmd(3), cmd(4)]);
    let path = dir.join(SEG_2);
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replacen("\"ts\":20,", "\"ts\":29,", 1)).unwrap();
    let mut seqs = Vec::new();
    let err = SegmentedLog::open(&dir, |entry| {
        seqs.extend(command_seq(&entry));
        Ok(())
    })
    .err()
    .unwrap();
    assert!(format!("{err:#}").contains(&format!("does not match its listing in {MANIFEST}")), "{err:#}");
    // Nothing of the altered segment, or after it, is handed on.
    assert_eq!(seqs, [0, 1]);
}

#[test]
fn opening_refuses_a_completed_segment_rewritten_while_it_is_read() {
    let dir = scratch_dir("open-rewritten");
    write(&dir, 2, &[cmd(0), cmd(1), cmd(2)]);
    let path = dir.join(SEG_0);
    let len = fs::metadata(&path).unwrap().len() as usize;
    // Once its first entry is handed on, the segment's later chunks change under the reader.
    let err = SegmentedLog::open(&dir, |entry| {
        if entry == cmd(0) {
            fs::write(&path, vec![b' '; len]).unwrap();
        }
        Ok(())
    })
    .err()
    .unwrap();
    assert!(format!("{err:#}").contains(&format!("does not match its listing in {MANIFEST}")), "{err:#}");
}

#[test]
fn a_log_file_is_handed_on_a_line_at_a_time() {
    let path = scratch_dir("file-streams").join("log.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, [line(&cmd(0)), line(&cmd(1))].concat()).unwrap();
    // A line appended once the first entry is handed on is read only if the file is read after it.
    let mut seen = Vec::new();
    read_file(&path, |entry| {
        if seen.is_empty() {
            append(&path, &line(&cmd(2)));
        }
        seen.push(entry);
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, [cmd(0), cmd(1), cmd(2)]);
}

// ── Helpers ──

fn cmd(seq: u64) -> LogEntry {
    LogEntry::Command(CommandEntry {
        sequenced: Sequenced { seq, ts: seq as i64 * 10, cmd: Command::Tick },
        events: Some(Vec::new()),
    })
}

fn withdrawal(nonce: u64) -> LogEntry {
    LogEntry::Withdrawal(SignedWithdrawal {
        account: AccountId([1; 20]),
        amount: Fixed::from_int(5),
        nonce,
        chain_id: 1,
        vault: AccountId([2; 20]),
        signature: "0x01".into(),
        approved_seq: None,
    })
}

fn checkpoint(seq: u64) -> LogEntry {
    LogEntry::Checkpoint(Checkpoint {
        parent: None,
        seq,
        root: "0x00".into(),
        signature: "0x01".into(),
        signer: "0x02".into(),
        domain: None,
        deferred_withdrawals: false,
    })
}

/// Appends `entries` through a writer reopened on `dir`.
fn write(dir: &Path, segment_commands: u64, entries: &[LogEntry]) {
    let manifest = watch::channel(Manifest::default()).0;
    let mut writer = SegmentedLog::open(dir, |_| Ok(())).unwrap().into_writer(segment_commands, manifest).unwrap();
    for entry in entries {
        writer.append(entry).unwrap();
    }
    writer.flush().unwrap();
}

/// The file names in `dir`, sorted.
fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

/// The seq of `entry` if it is a command.
fn command_seq(entry: &LogEntry) -> Option<u64> {
    match entry {
        LogEntry::Command(c) => Some(c.sequenced.seq),
        _ => None,
    }
}

/// `entry` as a log line.
fn line(entry: &LogEntry) -> Vec<u8> {
    [serde_json::to_vec(entry).unwrap(), b"\n".to_vec()].concat()
}

fn append(path: &Path, bytes: &[u8]) {
    fs::OpenOptions::new().append(true).open(path).unwrap().write_all(bytes).unwrap();
}
