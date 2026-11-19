//! Rewrites a log's entries in place, every other line keeping its bytes, and relists their
//! segments, as an archive that serves its own manifest for the files it serves does: a copy that
//! spells what the writer wrote otherwise, or drops or replaces it.

use sha2::{Digest, Sha256};

use crate::log::{Checkpoint, CommandEntry, LogEntry, MANIFEST, SegmentInfo};

/// `signature` in upper-case hex.
pub fn upper_case(signature: &str) -> String {
    let spelt = format!("0x{}", signature.trim_start_matches("0x").to_uppercase());
    assert_ne!(spelt, signature, "the signature holds no letter to spell otherwise");
    spelt
}

/// `signature` without its `0x`.
pub fn unprefixed(signature: &str) -> String {
    signature.trim_start_matches("0x").to_owned()
}

/// The command's seal, its signature in upper-case hex.
pub fn upper_case_seal(command: &mut CommandEntry) {
    let seal = command.seal.as_mut().expect("the sequencer seals every command");
    seal.signature = upper_case(&seal.signature);
}

/// Rewrites, in the log in `dir`, the command at `seq` through `edit`, and lists the rewritten
/// segment under its new hash, as an archive serves a manifest that matches its own files.
pub fn rewrite_command(dir: &std::path::Path, seq: u64, edit: impl Fn(&mut CommandEntry)) {
    rewrite(dir, seq, |entry| match entry {
        LogEntry::Command(command) if command.sequenced.seq == seq => {
            edit(command);
            true
        }
        _ => false,
    });
}

/// Leaves the deployment out of the log's last checkpoint in `dir`, as an archive may, since replay reads
/// it from the log and the signature binds it either way: the checkpoint, as it was.
pub fn drop_last_checkpoint_domain(dir: &std::path::Path) -> Checkpoint {
    let last = crate::test_support::log_entries(dir)
        .into_iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .expect("the log holds a checkpoint");
    assert!(last.domain.is_some(), "the checkpoint names no deployment to leave out");
    rewrite(dir, last.seq, |entry| match entry {
        LogEntry::Checkpoint(checkpoint) if checkpoint.seq == last.seq => {
            checkpoint.domain = None;
            true
        }
        _ => false,
    });
    last
}

/// Spells the signer of the log's last checkpoint in `dir` in mixed case, as an archive may: the
/// checkpoint, as it was.
pub fn rewrite_last_checkpoint_signer(dir: &std::path::Path) -> Checkpoint {
    let anchor = crate::test_support::log_entries(dir)
        .into_iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .expect("the log holds a checkpoint");
    rewrite(dir, anchor.seq, |entry| match entry {
        LogEntry::Checkpoint(checkpoint) if checkpoint.seq == anchor.seq => {
            let digits = checkpoint.signer.trim_start_matches("0x");
            let mixed: String = digits
                .chars()
                .enumerate()
                .map(|(n, digit)| if n % 2 == 0 { digit.to_ascii_uppercase() } else { digit })
                .collect();
            assert_ne!(mixed, digits, "the signer holds no letter to spell otherwise");
            checkpoint.signer = format!("0x{mixed}");
            true
        }
        _ => false,
    });
    anchor
}

/// Removes the last checkpoint of the log in `dir`, every other line keeping its bytes, and relists its
/// segment: the commands since the checkpoint before it become a tail no checkpoint covers.
pub fn drop_last_checkpoint(dir: &std::path::Path) {
    let last = crate::test_support::log_entries(dir)
        .into_iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint.seq),
            _ => None,
        })
        .expect("the log holds a checkpoint");
    let file = segment_of(dir, last);
    let bytes: Vec<u8> = std::fs::read(dir.join(&file))
        .unwrap()
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| !matches!(serde_json::from_slice(line), Ok(LogEntry::Checkpoint(checkpoint)) if checkpoint.seq == last))
        .flatten()
        .copied()
        .collect();
    relist(dir, &file, &bytes);
}

/// Rewrites the first entry `edit` changes in the segment of the log in `dir` holding `seq`, every
/// other line keeping its bytes, and relists the segment.
fn rewrite(dir: &std::path::Path, seq: u64, edit: impl Fn(&mut LogEntry) -> bool) {
    let file = segment_of(dir, seq);
    let (mut bytes, mut found) = (Vec::new(), false);
    for line in std::fs::read(dir.join(&file)).unwrap().split_inclusive(|byte| *byte == b'\n') {
        let mut entry: LogEntry = serde_json::from_slice(line).unwrap();
        if !found && edit(&mut entry) {
            bytes.extend(serde_json::to_vec(&entry).unwrap());
            bytes.push(b'\n');
            found = true;
        } else {
            bytes.extend_from_slice(line);
        }
    }
    assert!(found, "nothing at seq {seq} in {file} to rewrite");
    relist(dir, &file, &bytes);
}

/// The name of the segment of the log in `dir` that holds `seq`.
fn segment_of(dir: &std::path::Path, seq: u64) -> String {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|file| file.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| crate::log::segment_seq(name).is_some_and(|first| first <= seq))
        .collect();
    files.sort();
    files.pop().expect("a segment holds the seq")
}

/// Writes `bytes` as the segment `file` of the log in `dir`, and lists it under them if it is listed.
fn relist(dir: &std::path::Path, file: &str, bytes: &[u8]) {
    std::fs::write(dir.join(file), bytes).unwrap();
    let mut listings: Vec<SegmentInfo> = std::fs::read_to_string(dir.join(MANIFEST))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    if let Some(listed) = listings.iter_mut().find(|listed| listed.file == file) {
        listed.bytes = bytes.len() as u64;
        listed.sha256 = hex::encode(Sha256::digest(bytes));
        let text: String = listings.iter().map(|listed| serde_json::to_string(listed).unwrap() + "\n").collect();
        std::fs::write(dir.join(MANIFEST), text).unwrap();
    }
}
