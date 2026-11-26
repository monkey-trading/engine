//! The check of the log before a daily snapshot's checkpoint that a start from the snapshot makes before it serves
//! any of it (`verified_back`): against the links, activations and checkpoint roots the snapshot carries, through
//! the descriptors the node serves the segments by.

use std::path::{Path, PathBuf};

use engine_core::protocol::ProtocolVersion;

use super::carried_tests::{DOMAIN, snapshot_starts, whole_carried};
use crate::log::{LogEntry, Manifest, Offset, SegmentedLog};
use crate::sequencer::replay::{Behind, Carried};
use crate::test_support::{log_entries, scratch_dir};

/// A start from a snapshot checks the log before the snapshot's checkpoint: on a log that holds it all, from the
/// command after the first link on; one whose command there differs from the one the links name checks nothing, as
/// does one whose bytes differ from the descriptor the node read at its start; and a log whose earlier segments are
/// elsewhere, from the first it holds.
#[tokio::test]
async fn the_log_before_a_snapshot_is_checked_against_its_carried_links() {
    let (dir, carried) = funded("back");
    let first = first_checked(&carried);
    assert_eq!(back(&dir, &carried), Some(first), "the whole log checks from the command after the first link");

    let held = copied(&dir, "back-held");
    super::remove_segments_before(&held, carried.links.last().unwrap().seq);
    let first_held = log_entries(&held).iter().find_map(|entry| match entry {
        LogEntry::Command(command) => Some(command.sequenced.seq),
        _ => None,
    });
    assert_eq!(back(&held, &carried), first_held.map(|seq| seq.max(first)), "from the first command this host holds");

    let altered = copied(&dir, "back-altered");
    // The descriptors the node serves by, read at its start, before the host alters a segment and its listing.
    let served = held_manifest(&altered, &carried);
    let seq = rewrite(&altered, |lines| {
        let line = lines.iter().position(|line| line.contains("\"Tick\""))?;
        let mut entry: LogEntry = serde_json::from_str(&lines[line]).unwrap();
        let LogEntry::Command(command) = &mut entry else { return None };
        command.sequenced.ts += 1;
        let seq = command.sequenced.seq;
        lines[line] = serde_json::to_string(&entry).unwrap();
        Some(seq)
    });
    let in_window = first <= seq;
    assert_eq!(back(&altered, &carried).is_none(), in_window, "an altered command in the window checks nothing");
    let against_served =
        super::verified_back(&altered, behind(&carried), &served, (DOMAIN, &carried.checkpoint_signers));
    assert_eq!(against_served.is_none(), in_window, "bytes that differ from the descriptor served by check nothing");
}

/// The window's checkpoints and withdrawals hold their places: a log whose checkpoint the host moved before its own
/// command, or removed, or whose withdrawal it moved before the checkpoint the withdrawal's authorization names,
/// checks nothing, though every command's bytes are the ones the links name; so does a log the snapshot names no
/// version for that keeps every approval in the engine.
#[tokio::test]
async fn the_log_before_a_snapshot_is_checked_for_its_checkpoints_and_withdrawals() {
    let (dir, carried) = funded("back-places");
    let first = first_checked(&carried);
    let in_window = |seq: u64| first < seq && seq < carried.links.last().unwrap().seq;

    let moved = copied(&dir, "back-moved-checkpoint");
    let seq = rewrite(&moved, |lines| {
        let line = (1..lines.len()).find(|line| {
            checkpoint_seq(&lines[*line]).is_some_and(in_window) && lines[line - 1].contains("\"sequenced\"")
        })?;
        lines.swap(line - 1, line);
        checkpoint_seq(&lines[line - 1])
    });
    assert!(in_window(seq), "a checkpoint moved before its command at seq {seq}");
    assert_eq!(back(&moved, &carried), None, "a checkpoint before its own command checks nothing");

    let removed = copied(&dir, "back-removed-checkpoint");
    rewrite(&removed, |lines| {
        let line = lines.iter().position(|line| checkpoint_seq(line).is_some_and(in_window))?;
        checkpoint_seq(&lines.remove(line))
    });
    assert_eq!(back(&removed, &carried), None, "a log missing a checkpoint checks nothing");

    let early = copied(&dir, "back-early-withdrawal");
    rewrite(&early, |lines| {
        let (line, named) = lines.iter().enumerate().find_map(|(line, text)| {
            let LogEntry::Withdrawal(withdrawal) = serde_json::from_str(text).ok()? else { return None };
            Some((line, withdrawal.authorization?.checkpoint_seq)).filter(|(_, named)| in_window(*named))
        })?;
        let checkpoint = lines[..line].iter().position(|text| checkpoint_seq(text) == Some(named))?;
        let withdrawal = lines.remove(line);
        lines.insert(checkpoint, withdrawal);
        Some(named)
    });
    assert_eq!(back(&early, &carried), None, "a withdrawal before the checkpoint it names checks nothing");

    let untyped = Carried { activations: vec![(ProtocolVersion::V21, 0)], ..carried.clone() };
    assert_eq!(back(&dir, &untyped), None, "no version that keeps every approval");
}

// ── Helpers ──

/// A funded log in a scratch directory named for `name`, and the state a whole replay of it carries at its last
/// checkpoint a snapshot can stand at.
fn funded(name: &str) -> (PathBuf, Carried) {
    let dir = scratch_dir(&format!("carried-{name}"));
    crate::sequencer::fixture_tests::write_funded_log(&dir, 4);
    let entries = log_entries(&dir);
    let at = *snapshot_starts(&entries).last().unwrap();
    let carried = whole_carried(&entries).remove(&at).unwrap();
    (dir, carried)
}

/// What `carried` holds of the log before its checkpoint.
fn behind(carried: &Carried) -> Behind<'_> {
    Behind { links: &carried.links, activations: &carried.activations, roots: &carried.checkpoint_roots }
}

/// The first seq a check of a log that holds every segment starts at: after the first link and the activation of
/// the version that keeps every approval in the engine.
fn first_checked(carried: &Carried) -> u64 {
    let typed = carried.activations.iter().find(|(version, _)| *version >= ProtocolVersion::V22).unwrap().1;
    carried.links.first().unwrap().seq.max(typed) + 1
}

/// The descriptors a start from `carried`'s checkpoint serves the log in `dir` by.
fn held_manifest(dir: &Path, carried: &Carried) -> Manifest {
    let checkpoint = Offset::start_of(carried.links.last().unwrap().seq);
    SegmentedLog::open_from(dir, checkpoint, |_| Ok(())).unwrap().manifest_view()
}

/// `verified_back` over the log in `dir` as a start from `carried`'s checkpoint reads it.
fn back(dir: &Path, carried: &Carried) -> Option<u64> {
    let held = held_manifest(dir, carried);
    super::verified_back(dir, behind(carried), &held, (DOMAIN, &carried.checkpoint_signers))
}

/// A copy of the log files in `from`, in a scratch directory named for `name`.
fn copied(from: &Path, name: &str) -> PathBuf {
    let to = scratch_dir(&format!("carried-{name}"));
    std::fs::create_dir_all(&to).unwrap();
    for file in std::fs::read_dir(from).unwrap().map(Result::unwrap) {
        if file.file_type().unwrap().is_file() {
            std::fs::copy(file.path(), to.join(file.file_name())).unwrap();
        }
    }
    to
}

/// The seq of the checkpoint `line` holds, if it holds one.
fn checkpoint_seq(line: &str) -> Option<u64> {
    match serde_json::from_str(line).ok()? {
        LogEntry::Checkpoint(checkpoint) => Some(checkpoint.seq),
        _ => None,
    }
}

/// Edits the first segment of the log in `dir`, in file order, whose lines `edit` changes, and relists it as a host
/// that altered it would; what `edit` gives.
fn rewrite(dir: &Path, mut edit: impl FnMut(&mut Vec<String>) -> Option<u64>) -> u64 {
    let mut segments: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "jsonl") && path.file_name().unwrap() != crate::log::MANIFEST
        })
        .collect();
    segments.sort();
    for segment in segments {
        let mut lines: Vec<String> = std::fs::read_to_string(&segment).unwrap().lines().map(str::to_owned).collect();
        if let Some(found) = edit(&mut lines) {
            let bytes = lines.join("\n") + "\n";
            std::fs::write(&segment, &bytes).unwrap();
            relist(dir, segment.file_name().unwrap().to_str().unwrap(), bytes.as_bytes());
            return found;
        }
    }
    panic!("no segment of the log holds what the edit looks for")
}

/// Lists `bytes` as the segment `file` of the log in `dir`, as a host that altered it would.
fn relist(dir: &Path, file: &str, bytes: &[u8]) {
    use sha2::Digest;
    let manifest = Manifest::load(dir).unwrap();
    let text: String = manifest
        .segments
        .iter()
        .cloned()
        .map(|mut listed| {
            if listed.file == file {
                (listed.bytes, listed.sha256) = (bytes.len() as u64, hex::encode(sha2::Sha256::digest(bytes)));
            }
            serde_json::to_string(&listed).unwrap() + "\n"
        })
        .collect();
    std::fs::write(dir.join(crate::log::MANIFEST), text).unwrap();
}
