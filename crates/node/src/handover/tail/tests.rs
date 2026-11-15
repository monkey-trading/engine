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

/// A copy that followed the predecessor while it stopped holds the tail's first entries: the tail goes
/// on from the copy's last command, the entries through it are not taken again, and the copy ends as
/// the predecessor's log does.
#[tokio::test]
async fn a_copy_that_followed_while_the_predecessor_stopped_takes_only_the_rest_of_the_tail() {
    let (full, copy) = (scratch_dir("tail-past-copy-full"), scratch_dir("tail-past-copy-copy"));
    ticks(&full, 4).await;
    std::fs::create_dir_all(&copy).unwrap();
    for file in std::fs::read_dir(&full).unwrap().map(Result::unwrap) {
        std::fs::copy(file.path(), copy.join(file.file_name())).unwrap();
    }
    ticks(&full, 4).await;
    let signer = AccountId(crate::tee::EnclaveKey::test_key().address());
    let domain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };
    let mut follower =
        Follower::open(&copy, crate::test_support::SEGMENT_COMMANDS, domain, signer, [signer].into(), None, |_| {})
            .unwrap();
    let (tip, _) = follower.tip().unwrap();
    // The predecessor's tail past the tip it was asked from, of which following took the first lines.
    let mut previous = None;
    let mut tail = Vec::new();
    for entry in log_entries(&full) {
        let offset = offset_of(previous, &entry).unwrap();
        previous = Some(offset);
        if offset > Offset::start_of(tip) {
            tail.extend(serde_json::to_vec(&entry).unwrap());
            tail.push(b'\n');
        }
    }
    let lines: Vec<&[u8]> = tail.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).collect();
    let mut followed = Vec::new();
    for line in &lines[..3] {
        followed.extend_from_slice(line);
        followed.push(b'\n');
    }
    follower.take_tail(Offset::start_of(tip), &followed).unwrap();
    let (moved, _) = follower.tip().unwrap();
    assert!(moved > tip, "following took no command");
    let (after, rest) = past_copy(&follower, tip, &tail).unwrap();
    assert_eq!(after, Offset::start_of(moved));
    assert!(rest.len() < tail.len());
    follower.take_tail(after, &rest).unwrap();
    drop(follower);
    assert_eq!(log_entries(&copy), log_entries(&full), "the copy is not the predecessor's log");
}

/// Runs the test sequencer on `dir`, from its log, for `count` ticks, then stops it.
async fn ticks(dir: &std::path::Path, count: usize) {
    let node = TestSequencer::start(dir, 1024);
    for _ in 0..count {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
}
