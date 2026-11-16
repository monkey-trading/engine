//! A member answers an entries question the ring does not cover from its durable log: a gap older than
//! what it keeps, and an asker whose copy is empty.

use std::path::Path;

use engine_core::command::Command;

use super::tests::test_server;
use super::*;
use crate::log::offset_of;
use crate::test_support::{TestSequencer, log_entries, published_manifest, scratch_dir};

/// A copy with an empty ring that holds the log in `dir` on disk, as a member's does for what its ring
/// no longer keeps.
fn copy_over(dir: &Path) -> Copy {
    let copy = Copy::default();
    copy.logged(dir.to_owned(), published_manifest(dir));
    copy
}

/// The log in `dir`, each entry at its offset.
fn offsets(dir: &Path) -> Vec<(Offset, LogEntry)> {
    let mut previous = None;
    log_entries(dir)
        .into_iter()
        .map(|entry| {
            let offset = offset_of(previous, &entry).unwrap();
            previous = Some(offset);
            (offset, entry)
        })
        .collect()
}

async fn ticks(dir: &Path, count: usize) {
    let node = TestSequencer::start(dir, 1024);
    for _ in 0..count {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
}

#[tokio::test]
async fn entries_older_than_the_ring_and_from_the_first_come_from_the_durable_log() {
    let dir = scratch_dir("link-durable-entries");
    ticks(&dir, 8).await;
    let log = offsets(&dir);
    let copy = copy_over(&dir);
    assert_eq!(copy.entries_after(None), Vec::new(), "the ring holds nothing");
    assert_eq!(copy.entries(None).await, log, "an empty asker is given the log from its first entry");
    let after = log[2].0;
    assert_eq!(copy.entries(Some(after)).await, log[3..].to_vec(), "a gap beyond the ring is read from the log");
    let tip = log.last().unwrap().0;
    assert!(copy.entries(Some(tip)).await.is_empty(), "nothing past the log's tip");
    let largest = Some(Offset { seq: u64::MAX, index: u32::MAX });
    assert!(copy.entries(largest).await.is_empty(), "the largest offset answers nothing");
}

/// A log whose first entry is not seq 0's command cannot start an empty asker's chain: it is not served.
#[tokio::test]
async fn an_empty_asker_is_given_only_a_log_that_starts_at_its_first_entry() {
    let dir = scratch_dir("link-durable-from-first");
    ticks(&dir, 8).await;
    let ring = Copy::default();
    for (offset, entry) in offsets(&dir).into_iter().skip(2) {
        ring.appended(offset, &entry);
    }
    assert!(ring.entries(None).await.is_empty(), "a ring that begins after seq 0 does not serve an empty asker");
}

#[tokio::test]
async fn the_member_link_answers_entries_from_the_durable_log() {
    let dir = scratch_dir("link-durable-served");
    ticks(&dir, 8).await;
    let log = offsets(&dir);
    let mut server = test_server("member-link-durable");
    server.copy = copy_over(&dir);
    for (after, expected) in [(None, log.clone()), (Some(log[2].0), log[3..].to_vec())] {
        let Reply::Entries { entries } = server.answer(&Request::Entries { after }).await.unwrap() else {
            panic!("no entries")
        };
        assert_eq!(entries, expected, "after {after:?}");
    }
}
