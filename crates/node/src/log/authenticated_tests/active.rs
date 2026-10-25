//! Active-segment reads only decode bytes the writer committed.

use super::*;

#[test]
fn active_backfill_rejects_host_changes_after_startup_before_any_entry_is_decoded() {
    let mut f = fixture("active-startup-tamper", 100);
    f.writer.append(&command(0)).expect("append");
    f.writer.sync().expect("sync");
    drop(f.writer);
    let log = SegmentedLog::open(&f.dir, |_| Ok(())).expect("verified startup pass");
    let view = watch::channel(log.manifest_view()).1;
    let mut reader = LogReader::open(&f.dir, Offset::start_of(0), view).expect("reader opens");
    let path = f.dir.join("00000000000000000000.jsonl");
    let original = fs::read_to_string(&path).expect("read host file");
    let rewritten = original.replacen("\"ts\":0,", "\"ts\":9,", 1);
    assert_ne!(rewritten, original);
    fs::write(&path, rewritten).expect("host tamper");
    let error = reader.next_entry().expect_err("no unauthenticated first command");
    assert!(format!("{error:#}").contains("flushed prefix"));
    fs::write(path, original).expect("restore real source");
    assert_eq!(reader.next_entry().expect("verified source"), Some((Offset::start_of(0), command(0))));
}

#[test]
fn released_active_reader_reauthenticates_the_bytes_it_will_decode() {
    let mut f = fixture("active-release-tamper", 100);
    for seq in 0..12 {
        f.writer.append(&command(seq)).expect("append");
    }
    f.writer.flush().expect("flush");
    let mut reader = LogReader::open(&f.dir, Offset::start_of(0), f.view).expect("reader");
    assert_eq!(reader.next_entry().expect("first"), Some((Offset::start_of(0), command(0))));
    reader.release();
    let path = f.dir.join("00000000000000000000.jsonl");
    let text = fs::read_to_string(&path).expect("host bytes");
    let changed = text.replacen("\"ts\":1,", "\"ts\":9,", 1);
    assert_ne!(changed, text);
    fs::write(path, changed).expect("host rewrite");
    assert!(reader.next_entry().is_err(), "no rewritten second command");
}

#[test]
fn a_complete_host_append_remains_invisible_until_its_bytes_are_writer_committed() {
    let mut f = fixture("active-prefix-bound", 100);
    f.writer.append(&command(0)).expect("append");
    f.writer.flush().expect("flush");
    let path = f.dir.join("00000000000000000000.jsonl");
    let prefix_len = fs::metadata(&path).expect("metadata").len();
    let mut reader = LogReader::open(&f.dir, Offset::start_of(0), f.view).expect("reader");
    assert_eq!(offsets(&mut reader).expect("initial prefix"), [Offset::start_of(0)]);
    let bytes = line(&command(1));
    host_append(&path, &bytes[..10]);
    assert!(reader.next_entry().expect("uncommitted partial ignored").is_none());
    host_append(&path, &bytes[10..]);
    reader.release();
    assert!(reader.next_entry().expect("uncommitted complete line ignored").is_none());
    fs::OpenOptions::new().write(true).open(&path).expect("host open").set_len(prefix_len).expect("restore prefix");
    f.writer.append(&command(1)).expect("real writer append");
    f.writer.flush().expect("publish actual bytes");
    assert_eq!(reader.next_entry().expect("committed next"), Some((Offset::start_of(1), command(1))));
}

#[test]
fn active_to_completed_rotation_authenticates_the_completed_file_on_resumed_read() {
    let mut f = fixture("active-rotation-check", 2);
    f.writer.append(&command(0)).expect("append");
    f.writer.flush().expect("flush");
    let mut reader = LogReader::open(&f.dir, Offset::start_of(0), f.view).expect("reader");
    assert_eq!(offsets(&mut reader).expect("initial prefix"), [Offset::start_of(0)]);
    reader.release();
    for seq in 1..4 {
        f.writer.append(&command(seq)).expect("append with rotation");
    }
    f.writer.flush().expect("flush");
    let path = f.dir.join("00000000000000000000.jsonl");
    let text = fs::read_to_string(&path).expect("old active bytes");
    let changed = text.replacen("\"ts\":1,", "\"ts\":9,", 1);
    assert_ne!(text, changed);
    fs::write(path, changed).expect("completed-file rewrite");
    assert!(reader.next_entry().is_err());
}

#[test]
fn a_host_manifest_is_a_locator_and_cannot_supply_backfill_authority() {
    let mut f = fixture("active-unsigned-manifest", 1);
    for seq in 0..3 {
        f.writer.append(&command(seq)).expect("append");
    }
    f.writer.flush().expect("flush");
    let host = Manifest::load(&f.dir).expect("unsigned locator parse");
    assert!(host.authority.is_none());
    assert!(LogReader::open(&f.dir, Offset::start_of(0), watch::channel(host).1).is_err());
    let mut authenticated = LogReader::open(&f.dir, Offset::start_of(0), f.view).expect("writer authority");
    assert_eq!(
        offsets(&mut authenticated).expect("all actual bytes"),
        [Offset::start_of(0), Offset::start_of(1), Offset::start_of(2)]
    );
}

#[test]
fn checked_chunks_reject_a_rewrite_between_hashing_and_decoding() {
    use crate::log::checked::{CheckedChunks, check_prefix};
    use std::io::Read;

    let mut f = fixture("active-toctou", 100);
    for seq in 0..15 {
        f.writer.append(&command(seq)).expect("append");
    }
    f.writer.flush().expect("flush");
    let active = f.view.borrow().active.clone().expect("writer prefix");
    let path = f.dir.join(&active.info.file);
    let hashes = check_prefix(&path, &active.info).expect("first-pass commitment match");
    let text = fs::read_to_string(&path).expect("host bytes");
    let changed = text.replacen("\"ts\":12,", "\"ts\":99,", 1);
    assert_ne!(text, changed);
    fs::write(&path, changed).expect("host rewrite after first pass");
    let mut reader = CheckedChunks::open(&path, hashes, 0).expect("unchanged initial chunk");
    let mut decoded = Vec::new();
    assert!(reader.read_to_end(&mut decoded).is_err(), "later changed chunk never reaches a successful full decode");
}
