//! The tail's pieces: the versions a successor reads, the report data its tip binds, and the entries
//! a predecessor reads past a tip.

use super::*;

use engine_core::command::Command;

use crate::test_support::{TestSequencer, log_entries, scratch_dir};

/// A successor that reads only older protocol versions than the log runs is refused; one that reads
/// the log's version, or one this build does not know, which is newer, is not.
#[test]
fn a_successor_must_read_the_logs_protocol_version() {
    let log = ProtocolVersion::LATEST;
    ensure_reads(log, &version_name(log)).unwrap();
    ensure_reads(ProtocolVersion::V50, &version_name(log)).unwrap();
    ensure_reads(log, "v99999").unwrap();
    let err = ensure_reads(log, &version_name(ProtocolVersion::V50)).unwrap_err();
    assert!(format!("{err:#}").contains("newer than v50"), "{err:#}");
    assert!(ensure_reads(log, "latest").is_err(), "a name that is no version passed");
}

/// The tip and the versions it reads are bound in the report data: a host cannot move a successor's
/// tip, nor claim it reads newer versions, under the same quote.
#[test]
fn the_report_data_binds_the_tip_and_the_versions_read() {
    let domain = CheckpointDomain { chain_id: 31_337, vault: AccountId([7; 20]) };
    let tip = Tip { seq: 9, link: B256::repeat_byte(1) };
    let bound = |tip: &Tip| tail_report_data(domain, &[3; 32], &[4; 32], &[5; 32], "v51", tip);
    for moved in [Tip { seq: 10, ..tip.clone() }, Tip { link: B256::repeat_byte(2), ..tip.clone() }] {
        assert_ne!(bound(&moved), bound(&tip), "{moved:?} binds what {tip:?} does");
    }
    let newer = tail_report_data(domain, &[3; 32], &[4; 32], &[5; 32], "v52", &tip);
    assert_ne!(newer, bound(&tip), "a claim to read newer versions binds what the tip does");
    assert_ne!(bound(&tip), crate::handover::request_report_data(domain, &[3; 32], &[4; 32], &[5; 32], "v51"));
}

/// The predecessor reads exactly the entries its log holds after the tip's command, through the
/// final checkpoint, as the log holds them, and lists their count, size and hash.
#[tokio::test]
async fn the_tail_is_the_entries_after_the_tips_command() {
    let dir = scratch_dir("tail-read");
    let node = TestSequencer::start(&dir, 1024);
    for _ in 0..9 {
        node.submit(Command::Tick).await;
    }
    let (reply, stopped) = tokio::sync::oneshot::channel();
    node.tx.send(crate::sequencer::Request::FinalCheckpoint(reply)).await.unwrap();
    stopped.await.unwrap().unwrap();
    node.stop().await;
    let entries = log_entries(&dir);
    let LogEntry::Checkpoint(last) = entries.last().unwrap() else { panic!("the log ends at its final checkpoint") };
    let head = Head { seq: last.seq, root: last.root.clone() };
    let after = head.seq - 3;

    let (listing, bytes) = read_tail(&dir, after, &head).unwrap();
    let mut previous = None;
    let expected: Vec<_> = entries
        .into_iter()
        .filter(|entry| {
            let offset = offset_of(previous, entry).unwrap();
            previous = Some(offset);
            offset > Offset::start_of(after)
        })
        .collect();
    let read: Vec<LogEntry> = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(read, expected, "the tail is not the entries after seq {after}");
    assert_eq!((listing.after, listing.last_seq, listing.entries), (after, head.seq, expected.len() as u64));
    assert_eq!((listing.bytes, listing.sha256), (bytes.len() as u64, hex::encode(Sha256::digest(&bytes))));

    let err = read_tail(&dir, head.seq + 1, &head).unwrap_err();
    assert!(format!("{err:#}").contains("holds no command at seq"), "{err:#}");
}
