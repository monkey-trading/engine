//! A checkpoint's tail's pieces: the whole ranges before the checkpoint's, and the checkpoint's own up to and including
//! its entry, read from the writer's active prefix as from its completed segments.

use super::*;

use std::path::PathBuf;

use crate::archive::stand_in::checkpoints;

/// Each piece before the checkpoint's range is that whole range, as `range_bytes` builds it where a completed segment
/// holds it, and the checkpoint's piece is its range's lines up to and including the checkpoint's entry, read from the
/// writer's active prefix.
#[tokio::test]
async fn a_tails_pieces_are_whole_ranges_and_then_the_lines_through_its_checkpoint() {
    let dir = scratch_dir("archive-tail-pieces");
    let node = TestSequencer::start(&dir, 256);
    let (seq, view) = checkpoint_in_the_open_segment(&node, &dir, 3).await;
    let (last, whole) = (seq / RANGE_SEQS, whole_ranges(&view).unwrap());
    assert!(seq >= view.unlisted_from().unwrap(), "the checkpoint is in the writer's active prefix");
    let lines = log_lines(&dir);
    for index in 0..=last {
        let piece = piece_bytes(&dir, &view, index, seq).unwrap();
        let first = index * RANGE_SEQS;
        let expected: Vec<u8> = match index < last {
            true => joined(lines.iter().filter(|line| (first..first + RANGE_SEQS).contains(&line.seq))),
            false => {
                let through = lines.iter().position(|line| line.seq == seq && line.checkpoint).unwrap();
                joined(lines[..=through].iter().filter(|line| line.seq >= first))
            }
        };
        assert_eq!(piece, expected, "piece {index} of the tail at seq {seq}");
        if index < whole {
            assert_eq!(piece, range_bytes(&dir, &view, index).unwrap(), "piece {index} is its range");
        }
    }
    let piece = piece_bytes(&dir, &view, last, seq).unwrap();
    let entry = piece.split(|byte| *byte == b'\n').rfind(|line| !line.is_empty()).unwrap();
    let entry: LogEntry = serde_json::from_slice(entry).unwrap();
    assert!(matches!(entry, LogEntry::Checkpoint(checkpoint) if checkpoint.seq == seq), "it ends with the checkpoint");
    node.stop().await;
}

/// A piece is refused past the checkpoint's range, for a seq the copy holds no checkpoint at, for a range before the
/// checkpoint's that the copy holds no command after, and from an active prefix the host changed.
#[tokio::test]
async fn a_piece_the_copy_does_not_hold_is_refused() {
    let dir = scratch_dir("archive-tail-pieces-refused");
    let node = TestSequencer::start(&dir, 256);
    let (seq, view) = checkpoint_in_the_open_segment(&node, &dir, 2).await;
    node.stop().await;
    let last = seq / RANGE_SEQS;
    let e = piece_bytes(&dir, &view, last + 1, seq).unwrap_err();
    assert!(format!("{e:#}").contains("past the checkpoint"), "{e:#}");
    let held = checkpoints(&dir);
    let bare = (0..seq).rev().find(|bare| held.iter().all(|(at, _)| at != bare)).expect("a seq with no checkpoint");
    let e = piece_bytes(&dir, &view, bare / RANGE_SEQS, bare).unwrap_err();
    assert!(format!("{e:#}").contains(&format!("holds no checkpoint at seq {bare}")), "{e:#}");
    let flushed = view.active.as_ref().unwrap().last.seq;
    let e = piece_bytes(&dir, &view, flushed / RANGE_SEQS, flushed + RANGE_SEQS).unwrap_err();
    assert!(format!("{e:#}").contains("holds no command after"), "{e:#}");

    let altered = copy_of(&dir, "archive-tail-pieces-altered");
    let active = altered.join(&view.active.as_ref().unwrap().info.file);
    let mut raw = std::fs::read(&active).unwrap();
    raw[1] ^= 1;
    std::fs::write(&active, raw).unwrap();
    piece_bytes(&dir, &view, last, seq).unwrap();
    assert!(piece_bytes(&altered, &view, last, seq).is_err(), "an active prefix the host changed");
}

// ── Helpers ──

/// One line of the log, the seq whose entries it is among, and whether it is that seq's checkpoint.
struct Line {
    seq: u64,
    checkpoint: bool,
    bytes: Vec<u8>,
}

/// `lines` joined back into the log's bytes.
fn joined<'a>(lines: impl Iterator<Item = &'a Line>) -> Vec<u8> {
    lines.flat_map(|line| line.bytes.clone()).collect()
}

/// Ticks `node` until the writer's active prefix in `dir` holds a checkpoint in range `ranges` or past it, and returns
/// its seq and the writer's view then.
async fn checkpoint_in_the_open_segment(node: &TestSequencer, dir: &Path, ranges: u64) -> (u64, Manifest) {
    loop {
        node.submit(Command::Tick).await;
        let view = node.manifest.borrow().clone();
        let completed = view.unlisted_from().unwrap();
        let open = checkpoints(dir).into_iter().rfind(|(seq, _)| *seq >= completed && *seq >= ranges * RANGE_SEQS);
        if let Some((seq, _)) = open.filter(|_| view.active.is_some()) {
            return (seq, view);
        }
    }
}

/// Every line of the log in `dir`, its segment files read in order.
fn log_lines(dir: &Path) -> Vec<Line> {
    let mut seq = 0;
    segment_files(dir)
        .into_iter()
        .flat_map(|path| {
            std::fs::read(path).unwrap().split_inclusive(|byte| *byte == b'\n').map(<[u8]>::to_vec).collect::<Vec<_>>()
        })
        .map(|bytes| {
            let entry: LogEntry = serde_json::from_slice(&bytes).unwrap();
            let checkpoint = matches!(entry, LogEntry::Checkpoint(_));
            if let LogEntry::Command(command) = entry {
                seq = command.sequenced.seq;
            }
            Line { seq, checkpoint, bytes }
        })
        .collect()
}

/// The segment files of the log in `dir`, in order.
fn segment_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|file| file.unwrap().path())
        .filter(|path| crate::log::segment_seq(&path.file_name().unwrap().to_string_lossy()).is_some())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// A copy of the segment files of the log in `dir`, in a new directory named for `name`.
fn copy_of(dir: &Path, name: &str) -> PathBuf {
    let copy = scratch_dir(name);
    std::fs::create_dir_all(&copy).unwrap();
    for path in segment_files(dir) {
        std::fs::copy(&path, copy.join(path.file_name().unwrap())).unwrap();
    }
    copy
}
