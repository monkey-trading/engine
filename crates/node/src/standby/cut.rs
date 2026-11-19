//! A copy cut back to one of its checkpoints: a takeover's, to the checkpoint it resumes from, and a
//! replica's, to the last checkpoint before a tail no accepted signer sealed. Each cut runs behind a
//! `takeover.json` marker, from which everything that reads or extends a copy first finishes a cut a crash
//! interrupted (`finish`): an open of the copy (`open_copy_replayed`), a whole-log take
//! (`handover::successor::take_with`), the archive restore before one (`handover::sync_predecessor_log`)
//! and a takeover's start (`node::keys::cut_to_finalized_anchor`). The truncation lists the copy's end
//! state before it removes any file.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::checkpoints::Takeover;
use crate::log::{MANIFEST, segment_seq, truncate_to_checkpoint};

/// Truncates the copy in `dir` after the checkpoint `to` names, behind the marker.
/// Error cases:
/// - the marker cannot be written, synced or removed, or the truncation fails
pub(crate) fn cut_marked(dir: &Path, to: &Takeover) -> anyhow::Result<()> {
    // Published whole, or not at all: a marker a crash cut short would block every open.
    let staged = dir.join("takeover.json.staging");
    std::fs::write(&staged, serde_json::to_string(to).context("encoding the takeover marker")?)
        .context("writing the takeover marker")?;
    std::fs::File::open(&staged)?.sync_all().context("syncing the takeover marker")?;
    std::fs::rename(&staged, dir.join("takeover.json")).context("publishing the takeover marker")?;
    // The marker is listed before the truncation replaces the manifest.
    sync(dir)?;
    truncate_to_checkpoint(dir, to.seq, &to.root)?;
    unmark(dir)
}

/// Finishes a cut the marker in `dir` names, if one is there: whether or not what the crash left opens,
/// since a cut interrupted after it removed the later segments, before it trimmed its checkpoint's,
/// leaves a copy that opens holding part of what it cut.
/// Error cases:
/// - the marker is there but cannot be read, parsed or removed, or the truncation fails
pub(crate) fn finish(dir: &Path) -> anyhow::Result<()> {
    let bytes = match std::fs::read(dir.join("takeover.json")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        read => read.context("reading the interrupted takeover marker")?,
    };
    let to: Takeover = serde_json::from_slice(&bytes).context("parsing the interrupted takeover marker")?;
    truncate_to_checkpoint(dir, to.seq, &to.root).context("repairing the interrupted takeover")?;
    unmark(dir)
}

/// Removes the marker from `dir` for good, before the copy can grow past its checkpoint: a marker a power
/// loss brought back would cut the copy again.
/// Error cases:
/// - the marker cannot be removed, or the directory synced
fn unmark(dir: &Path) -> anyhow::Result<()> {
    std::fs::remove_file(dir.join("takeover.json")).context("removing the takeover marker")?;
    sync(dir)
}

/// Copies the copy's segment files in `dir` that may hold seq `from` or a later one, with its manifest,
/// to `<dir>.unaccepted-<from>`, over what an earlier open left there: what a cut back from `from`
/// drops, kept as evidence. Returns where they went.
/// Error cases:
/// - the copy cannot be listed, or a file cannot be copied or synced
pub(super) fn keep_tail(dir: &Path, from: u64) -> anyhow::Result<PathBuf> {
    let mut aside = dir.as_os_str().to_owned();
    aside.push(format!(".unaccepted-{from}"));
    let aside = PathBuf::from(aside);
    if aside.exists() {
        std::fs::remove_dir_all(&aside).with_context(|| format!("removing {}", aside.display()))?;
    }
    std::fs::create_dir_all(&aside).with_context(|| format!("creating {}", aside.display()))?;
    let mut segments = Vec::new();
    for file in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let file = file?;
        if let Some(first) = segment_seq(&file.file_name().to_string_lossy()) {
            segments.push((first, file.file_name()));
        }
    }
    segments.sort();
    let start = segments.iter().rposition(|(first, _)| *first <= from).unwrap_or(0);
    let manifest = dir.join(MANIFEST).exists().then_some(std::ffi::OsString::from(MANIFEST));
    for name in segments.split_off(start).into_iter().map(|(_, name)| name).chain(manifest) {
        let to = aside.join(&name);
        std::fs::copy(dir.join(&name), &to).with_context(|| format!("copying {}", to.display()))?;
        std::fs::File::open(&to)?.sync_all().with_context(|| format!("syncing {}", to.display()))?;
    }
    sync(&aside)?;
    // The evidence is listed beside the copy before the cut it precedes removes anything.
    sync(aside.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new(".")))?;
    Ok(aside)
}

/// Forces the directory's entries to storage.
/// Error cases:
/// - the directory cannot be opened or synced
fn sync(dir: &Path) -> anyhow::Result<()> {
    std::fs::File::open(dir)?.sync_all().with_context(|| format!("syncing {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A marker that is there but cannot be read stops the open rather than reading as none, which would
    /// let the copy grow under a cut that a later read would finish.
    #[test]
    fn a_marker_that_cannot_be_read_is_not_taken_for_none() {
        let dir = crate::test_support::scratch_dir("cut-unreadable-marker");
        std::fs::create_dir_all(dir.join("takeover.json")).unwrap();
        let refused = finish(&dir).expect_err("an unreadable marker read as none");
        assert!(format!("{refused:#}").contains("reading the interrupted takeover marker"), "{refused:#}");
        assert!(finish(&crate::test_support::scratch_dir("cut-no-marker")).is_ok(), "no marker refused the open");
    }
}
