use super::*;

use std::path::Path;

use engine_core::command::Command;
use tokio::sync::watch;

use crate::log::{LogEntry, SegmentedLog};
use crate::test_support::{TestSequencer, log_entries, published_manifest, scratch_dir};

mod pieces;

/// A range is the same bytes from every copy of the log, however its segments are cut: the sequencer's own,
/// one cut every five commands, one whose open segment was sealed early, as a handover seals the
/// predecessor's, and one whose single completed segment spans every range. Each range starts at its first seq's command, and the ranges together are the log's lines
/// up to the last whole one.
#[tokio::test]
async fn a_range_is_the_same_bytes_however_the_copy_is_cut() {
    let dir = scratch_dir("archive-range-written");
    let node = TestSequencer::start(&dir, 256);
    while node.manifest.borrow().unlisted_from().unwrap() < 10 * RANGE_SEQS {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
    let entries = log_entries(&dir);
    let (fives, sealed) = (scratch_dir("archive-range-fives"), scratch_dir("archive-range-sealed"));
    resegmented(&entries, &fives, 5, None);
    // A handover seals the open segment after the final checkpoint, before the successor's first command.
    let seal_after = (entries.len() / 2..entries.len() - 1)
        .find(|n| matches!(entries[n + 1], LogEntry::Command(_)))
        .expect("a command past the middle");
    resegmented(&entries, &sealed, 4, Some(seal_after));
    let long = scratch_dir("archive-range-long");
    resegmented(&entries, &long, u64::MAX, Some(seal_after));
    let copies = [&dir, &fives, &sealed, &long];
    let views: Vec<Manifest> = copies.iter().map(|dir| published_manifest(dir).borrow().clone()).collect();
    // Each copy's last segment is open, so each makes a different number of ranges whole.
    let whole = views.iter().map(|view| whole_ranges(view).unwrap()).min().unwrap();
    assert!(whole >= 4, "{whole} whole ranges");
    let lines: Vec<u8> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|file| file.unwrap().path())
        .filter(|path| crate::log::segment_seq(&path.file_name().unwrap().to_string_lossy()).is_some())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .flat_map(|path| std::fs::read(path).unwrap())
        .collect();
    let mut joined = Vec::new();
    for index in 0..whole {
        let ranges: Vec<Vec<u8>> =
            copies.iter().zip(&views).map(|(dir, view)| range_bytes(dir, view, index).unwrap()).collect();
        assert_eq!(ranges[0], ranges[1], "range {index}, cut every five commands");
        assert_eq!(ranges[0], ranges[2], "range {index}, sealed early");
        assert_eq!(ranges[0], ranges[3], "range {index}, read a line at a time from one long segment");
        let first: LogEntry = serde_json::from_slice(ranges[0].split(|byte| *byte == b'\n').next().unwrap()).unwrap();
        assert!(
            matches!(&first, LogEntry::Command(command) if command.sequenced.seq == index * RANGE_SEQS),
            "range {index} starts at its first seq's command"
        );
        joined.extend_from_slice(&ranges[0]);
    }
    assert_eq!(joined, lines[..joined.len()], "the ranges are the log's lines in order");
    let next = lines[joined.len()..].split(|byte| *byte == b'\n').next().unwrap();
    let next: LogEntry = serde_json::from_slice(next).unwrap();
    assert!(matches!(next, LogEntry::Command(command) if command.sequenced.seq == whole * RANGE_SEQS));
}

/// A range is refused before a completed segment holds its last seq, and when a segment it reads differs from
/// its listing.
#[tokio::test]
async fn a_range_not_yet_whole_or_read_from_a_changed_segment_is_refused() {
    let dir = scratch_dir("archive-range-refused");
    let node = TestSequencer::start(&dir, 256);
    while node.manifest.borrow().unlisted_from().unwrap() < 2 * RANGE_SEQS {
        node.submit(Command::Tick).await;
    }
    let view = node.manifest.borrow().clone();
    let whole = whole_ranges(&view).unwrap();
    assert!(range_bytes(&dir, &view, whole).is_err(), "range {whole} is not whole");
    range_bytes(&dir, &view, 0).unwrap();
    let listed = view.completed_at(0).unwrap();
    let mut raw = std::fs::read(dir.join(&listed.file)).unwrap();
    raw[1] ^= 1;
    std::fs::write(dir.join(&listed.file), raw).unwrap();
    assert!(range_bytes(&dir, &view, 0).is_err(), "a segment the host changed");
    node.stop().await;
}

/// A copy's checkpoint at a seq is read from its completed segments: its root where the copy holds one, none at a
/// seq it holds no checkpoint for, and an error for a seq no completed segment holds yet.
#[tokio::test]
async fn a_copys_checkpoint_is_read_from_its_completed_segments() {
    let dir = scratch_dir("archive-range-checkpoint");
    let node = TestSequencer::start(&dir, 256);
    while node.manifest.borrow().unlisted_from().unwrap() < 3 * RANGE_SEQS {
        node.submit(Command::Tick).await;
    }
    let view = node.manifest.borrow().clone();
    let completed = view.unlisted_from().unwrap();
    let written: Vec<(u64, String)> = log_entries(&dir)
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) if checkpoint.seq < completed => Some((checkpoint.seq, checkpoint.root)),
            _ => None,
        })
        .collect();
    assert!(written.len() >= 2, "{written:?}");
    for (seq, root) in &written {
        assert_eq!(checkpoint_root(&dir, &view, *seq).unwrap().as_ref(), Some(root), "the checkpoint at seq {seq}");
    }
    let bare = (0..completed).find(|seq| written.iter().all(|(at, _)| at != seq)).unwrap();
    assert_eq!(checkpoint_root(&dir, &view, bare).unwrap(), None, "seq {bare} has no checkpoint");
    assert!(checkpoint_root(&dir, &view, completed).is_err(), "the open segment is not read");
    node.stop().await;
}

/// An object holds a range when it spells the range's entries otherwise, as a copy backfilled from another
/// archive may (hex signatures in upper case, a checkpoint's deployment left out), and does not when it holds
/// another history.
#[tokio::test]
async fn a_respelt_copy_of_a_range_holds_it_and_another_history_does_not() {
    use crate::archive::{compress, holds};
    let dir = scratch_dir("archive-range-respelt");
    let node = TestSequencer::start(&dir, 256);
    while node.manifest.borrow().unlisted_from().unwrap() < 3 * RANGE_SEQS {
        node.submit(Command::Tick).await;
    }
    let raw = range_bytes(&dir, &node.manifest.borrow(), 1).unwrap();
    node.stop().await;
    let entries: Vec<LogEntry> = raw
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let encoded = |entries: &[LogEntry]| -> Vec<u8> {
        entries.iter().flat_map(|entry| [serde_json::to_vec(entry).unwrap(), b"\n".to_vec()].concat()).collect()
    };
    assert_eq!(encoded(&entries), raw, "the range's lines are its entries as they serialize");
    let mut respelt = entries.clone();
    let mut checkpoints = 0;
    for entry in &mut respelt {
        if let LogEntry::Checkpoint(checkpoint) = entry {
            checkpoint.signature = format!("0x{}", checkpoint.signature[2..].to_uppercase());
            checkpoint.domain = None;
            checkpoints += 1;
        }
    }
    assert!(checkpoints > 0, "the range holds a checkpoint");
    assert_ne!(encoded(&respelt), raw);
    assert!(holds(&compress(&encoded(&respelt)).unwrap(), &raw), "the same history spelt otherwise");
    let mut other = entries;
    if let LogEntry::Command(command) = &mut other[0] {
        command.sequenced.ts += 1;
    }
    assert!(!holds(&compress(&encoded(&other)).unwrap(), &raw), "another history");
}

// ── Helpers ──

/// Writes `entries` into a new log in `dir`, a segment every `commands` commands, its open segment sealed early
/// after the entry at `seal_after`, as a handover seals the predecessor's.
pub(crate) fn resegmented(entries: &[LogEntry], dir: &Path, commands: u64, seal_after: Option<usize>) {
    std::fs::create_dir_all(dir).unwrap();
    let log = SegmentedLog::open(dir, |_| Ok(())).unwrap();
    let mut writer = log.into_writer(commands, watch::channel(Default::default()).0).unwrap();
    for (n, entry) in entries.iter().enumerate() {
        writer.append(entry).unwrap();
        if seal_after == Some(n) {
            writer.flush().unwrap();
            writer.seal_open_segment().unwrap();
        }
    }
    writer.flush().unwrap();
}
