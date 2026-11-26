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
    let (first, last) = (first_checked(&carried), carried.links.last().unwrap().seq);
    assert_eq!(back(&dir, &carried), Some(first), "the whole log checks from the command after the first link");

    let held = copied(&dir, "back-held");
    super::remove_segments_before(&held, last);
    let first_held = segments(&held).into_iter().flat_map(|(_, lines)| lines).find_map(|line| command_seq(&line));
    assert_eq!(back(&held, &carried), first_held.map(|seq| seq.max(first)), "from the first command this host holds");

    let altered = copied(&dir, "back-altered");
    // The descriptors the node serves by, read at its start, before the host alters a segment and its listing.
    let served = held_manifest(&altered, &carried);
    let seq = edit(&altered, (last, "a tick"), |lines| {
        let line = lines.iter().position(|line| line.contains(r#""type":"tick""#))?;
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

/// The window's checkpoints and withdrawals hold their places: a log where the host put a checkpoint after a command
/// other than its own, or removed one, or put a withdrawal before the checkpoint its authorization names, checks
/// nothing, though every command's bytes are the ones the links name; so does a log the snapshot names no version for
/// that keeps every approval in the engine.
#[tokio::test]
async fn the_log_before_a_snapshot_is_checked_for_its_checkpoints_and_withdrawals() {
    let (dir, carried) = funded("back-places");
    let (first, last) = (first_checked(&carried), carried.links.last().unwrap().seq);
    let in_window = |seq: u64| first < seq && seq < last;

    let entries = segments(&dir).into_iter().flat_map(|(_, lines)| lines).collect::<Vec<_>>();
    let checkpoint = entries.iter().find(|line| checkpoint_seq(line).is_some_and(in_window)).unwrap().clone();
    let typed = entries.iter().find_map(|line| {
        let LogEntry::Withdrawal(withdrawal) = serde_json::from_str(line).ok()? else { return None };
        let named = withdrawal.authorization?.checkpoint_seq;
        let approved = withdrawal.approved_seq?;
        in_window(named).then(|| (line.clone(), approved, named))
    });
    let (withdrawal, approved, named) = typed.expect("a typed withdrawal naming a checkpoint in the window");

    let moved = copied(&dir, "back-moved-checkpoint");
    let at = edit(&moved, (last, "a command other than the checkpoint's"), |lines| {
        let elsewhere = |seq: u64| in_window(seq) && Some(seq) != checkpoint_seq(&checkpoint);
        let after = lines.iter().position(|line| command_seq(line).is_some_and(elsewhere))?;
        lines.insert(after + 1, checkpoint.clone());
        command_seq(&lines[after])
    });
    assert!(in_window(at), "a checkpoint after the command at seq {at}");
    assert_eq!(back(&moved, &carried), None, "a checkpoint after another command than its own checks nothing");

    let removed = copied(&dir, "back-removed-checkpoint");
    edit(&removed, (last, "a checkpoint"), |lines| {
        let line = lines.iter().position(|line| checkpoint_seq(line).is_some_and(in_window))?;
        checkpoint_seq(&lines.remove(line))
    });
    assert_eq!(back(&removed, &carried), None, "a log missing a checkpoint checks nothing");

    let early = copied(&dir, "back-early-withdrawal");
    edit(&early, (last, "a command after the approval and before the checkpoint it names"), |lines| {
        let unchecked = |seq: u64| in_window(seq) && approved <= seq && seq <= named;
        let after = lines.iter().position(|line| command_seq(line).is_some_and(unchecked))?;
        lines.insert(after + 1, withdrawal.clone());
        command_seq(&lines[after])
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

/// Each segment of the log in `dir` this host holds, in the manifest's order, with its lines.
fn segments(dir: &Path) -> Vec<(crate::log::SegmentInfo, Vec<String>)> {
    let listed = Manifest::load(dir).unwrap().segments.to_vec();
    let held = listed.into_iter().filter_map(|info| Some((std::fs::read_to_string(dir.join(&info.file)).ok()?, info)));
    held.map(|(text, info)| (info, text.lines().map(str::to_owned).collect())).collect()
}

/// Edits the first segment of the log in `dir`, in the manifest's order, whose lines `change` changes, of those that
/// end before `before`, the snapshot's command, so a start still opens the log from the snapshot; relists it as a host
/// that altered it would; and gives what `change` gives. `what` names what `change` looks for.
fn edit(dir: &Path, (before, what): (u64, &str), mut change: impl FnMut(&mut Vec<String>) -> Option<u64>) -> u64 {
    for (info, mut lines) in segments(dir).into_iter().filter(|(info, _)| info.last_seq < before) {
        if let Some(found) = change(&mut lines) {
            let bytes = lines.join("\n") + "\n";
            std::fs::write(dir.join(&info.file), &bytes).unwrap();
            relist(dir, &info.file, bytes.as_bytes());
            return found;
        }
    }
    panic!("no segment of the log before seq {before} holds {what}")
}

/// The seq of the command `line` holds, if it holds one.
fn command_seq(line: &str) -> Option<u64> {
    match serde_json::from_str(line).ok()? {
        LogEntry::Command(command) => Some(command.sequenced.seq),
        _ => None,
    }
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
