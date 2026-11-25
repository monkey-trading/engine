//! A backfill from before the first segment this host holds is refused before the connection upgrades,
//! naming the archive.

use engine_core::command::{Command, Sequenced};
use tokio::sync::watch;

use super::start::ensure_held;
use super::*;
use crate::log::{CommandEntry, LogEntry, SegmentedLog};
use crate::test_support::scratch_dir;

/// A log of segments 0, 2 and 4 whose first is not on this host.
fn archived(name: &str) -> PathBuf {
    let dir = scratch_dir(name);
    let mut writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, watch::channel(Manifest::default()).0).unwrap();
    for seq in 0..6 {
        let sequenced = Sequenced { seq, ts: 1, cmd: Command::Tick };
        writer.append(&LogEntry::Command(CommandEntry { seal: None, sequenced, events: Some(Vec::new()) })).unwrap();
    }
    writer.flush().unwrap();
    std::fs::remove_file(dir.join("00000000000000000000.jsonl")).unwrap();
    dir
}

/// A start the segments here reach back to is served, one before them, or before the first seq the node's start
/// verified, is refused with 400 naming the archive, and a log that holds everything it lists refuses nothing.
#[tokio::test]
async fn a_backfill_from_before_the_first_segment_here_is_refused_naming_the_archive() {
    let dir = archived("stream-archived");
    let all = Manifest::default();
    ensure_held(&dir, &all, Some(Offset::start_of(2))).await.unwrap();
    ensure_held(&dir, &all, Some(Offset::start_of(5))).await.unwrap();
    for from in [0, 1] {
        let Err(ApiError::BadRequest(refusal)) = ensure_held(&dir, &all, Some(Offset::start_of(from))).await else {
            panic!("seq {from} was served");
        };
        assert!(refusal.contains("public archive") && refusal.contains("seq 2"), "{refusal}");
    }
    let whole = scratch_dir("stream-whole");
    ensure_held(&whole, &all, Some(Offset::start_of(0))).await.unwrap();
    // A start from a snapshot verified nothing before seq 4: the host's segments before it are not served.
    let verified = Manifest { verified_from: 4, ..Manifest::default() };
    ensure_held(&whole, &verified, Some(Offset::start_of(4))).await.unwrap();
    let Err(ApiError::BadRequest(refusal)) = ensure_held(&whole, &verified, Some(Offset::start_of(3))).await else {
        panic!("seq 3 was served");
    };
    assert!(refusal.contains("public archive") && refusal.contains("seq 4"), "{refusal}");
}
