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
    assert_eq!(copy.entries(None).await.unwrap(), log, "an empty asker is given the log from its first entry");
    let after = log[2].0;
    assert_eq!(
        copy.entries(Some(after)).await.unwrap(),
        log[3..].to_vec(),
        "a gap beyond the ring is read from the log"
    );
    let tip = log.last().unwrap().0;
    assert!(copy.entries(Some(tip)).await.unwrap().is_empty(), "nothing past the log's tip");
    let largest = Some(Offset { seq: u64::MAX, index: u32::MAX });
    assert!(copy.entries(largest).await.unwrap().is_empty(), "the largest offset answers nothing");
}

/// A ring that begins after seq 0's command cannot start an empty asker's chain: it is not served.
#[tokio::test]
async fn a_ring_that_begins_after_the_first_entry_does_not_serve_an_empty_asker() {
    let dir = scratch_dir("link-durable-from-first");
    ticks(&dir, 8).await;
    let ring = Copy::default();
    for (offset, entry) in offsets(&dir).into_iter().skip(2) {
        ring.appended(offset, &entry);
    }
    assert!(
        ring.entries(None).await.unwrap().is_empty(),
        "a ring that begins after seq 0 does not serve an empty asker"
    );
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

/// A copy that trails by more than a page is given the gap page by page, each within `ENTRIES_BYTES` and
/// chaining onto the last, until it holds the whole log.
#[tokio::test]
async fn a_gap_larger_than_a_page_is_served_in_bounded_pages_that_chain() {
    let dir = scratch_dir("link-durable-pages");
    ticks(&dir, 150).await;
    let log = offsets(&dir);
    let copy = copy_over(&dir);
    let (mut after, mut served, mut pages) = (None, Vec::new(), 0);
    loop {
        let page = copy.entries(after).await.unwrap();
        if page.is_empty() {
            break;
        }
        let bytes: usize = page.iter().map(|(_, entry)| serde_json::to_vec(entry).unwrap().len()).sum();
        assert!(bytes <= ENTRIES_BYTES, "a page of {bytes} bytes exceeds the bound");
        after = page.last().map(|(offset, _)| *offset);
        served.extend(page);
        pages += 1;
    }
    assert!(pages > 1, "the log fits one page: the bound was not reached");
    assert_eq!(served, log, "the pages chain into the whole log");
}

/// A ring that begins at a command whose predecessor's checkpoint was evicted does not answer an asker
/// that lacks the checkpoint: the checkpoint restarts the seal chain, so the page is read from the log.
#[tokio::test]
async fn a_ring_that_lost_the_checkpoint_before_its_first_command_gives_way_to_the_log() {
    let dir = scratch_dir("link-durable-evicted-checkpoint");
    ticks(&dir, 12).await;
    let log = offsets(&dir);
    let at = log.iter().position(|(_, entry)| matches!(entry, LogEntry::Checkpoint(_))).expect("a checkpoint");
    let (after, next) = (log[at - 1].0, at + 1);
    assert!(matches!(log[next].1, LogEntry::Command(_)), "a command follows the checkpoint");
    let copy = copy_over(&dir);
    log[next..].iter().for_each(|(offset, entry)| copy.appended(*offset, entry));
    assert_eq!(copy.entries(Some(after)).await.unwrap(), log[at..].to_vec(), "the asker is given the checkpoint first");
}

/// A copy that keeps only its newest segments answers from them, and refuses an asker whose entries are in
/// the log's archive, naming it, not with an empty page the asker would take for the log's end.
#[tokio::test]
async fn entries_in_the_archive_are_refused_naming_it() {
    let dir = scratch_dir("link-durable-archived");
    let (manifest, published) = tokio::sync::watch::channel(crate::log::Manifest::default());
    let mut writer = crate::log::SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, manifest).unwrap();
    for seq in 0..6 {
        let sequenced = engine_core::command::Sequenced { seq, ts: 1, cmd: Command::Tick };
        writer
            .append(&LogEntry::Command(crate::log::CommandEntry { seal: None, sequenced, events: Some(Vec::new()) }))
            .unwrap();
    }
    writer.flush().unwrap();
    std::fs::remove_file(dir.join("00000000000000000000.jsonl")).unwrap();
    let copy = Copy::default();
    copy.logged(dir.clone(), published.clone());
    for after in [None, Some(Offset::start_of(0)), Some(Offset::start_of(1))] {
        let error = copy.entries(after).await.err().unwrap_or_else(|| panic!("{after:?} was answered"));
        assert!(error.is::<crate::log::NotLocal>(), "{after:?}: {error:#}");
        assert!(format!("{error:#}").contains("public archive"), "{after:?}: {error:#}");
    }
    let page = copy.entries(Some(Offset::start_of(3))).await.unwrap();
    assert_eq!(page.iter().map(|(offset, _)| offset.seq).collect::<Vec<_>>(), [4, 5]);
}
