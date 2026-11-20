//! A read of a segmented log from the segment holding a given command to the log's end, for a replay of
//! one range of it: none of the segments before is read, and each completed segment read is checked
//! against its listing in the manifest before any of it reaches the visitor, as a whole read checks it
//! (`SegmentedLog::open`).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};

use crate::log::segments::{Segment, list_segments};
use crate::log::{LogEntry, MANIFEST, Manifest};

/// The segment files in `dir`, listed before its manifest is read.
/// Error cases:
/// - the directory cannot be listed
pub(super) fn files(dir: &Path) -> anyhow::Result<Vec<(u64, PathBuf)>> {
    list_segments(dir)
}

/// Hands `visit` each entry of the log in `dir`, whose segment files are `files`, from the first of the
/// segment holding the command at `from` to the log's end, with the index in `files` of its segment.
/// Error cases:
/// - the manifest cannot be read, lists more segments than `files`, or leaves out one another follows
/// - a segment cannot be read, holds an invalid entry, or differs from its listing (as `SegmentedLog::open`)
/// - `visit` fails, with `Stop` to end the read early
pub(super) fn span(
    dir: &Path,
    files: &[(u64, PathBuf)],
    from: u64,
    visit: &mut dyn FnMut(usize, LogEntry) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let (manifest, _) = Manifest::read(dir)?;
    ensure!(
        manifest.segments.len() <= files.len(),
        "{MANIFEST} lists {} segments but {} are in {}",
        manifest.segments.len(),
        files.len(),
        dir.display()
    );
    let first = files.iter().rposition(|(first_seq, _)| *first_seq <= from).unwrap_or(0);
    let mut last = None;
    for (at, (first_seq, path)) in files.iter().enumerate().skip(first) {
        let listed = manifest.segments.get(at);
        if listed.is_none() && at + 1 < files.len() {
            bail!("{} is followed by another segment but is not in {MANIFEST}", path.display());
        }
        let segment = Segment::read(*first_seq, path, listed, &mut last, &mut |entry| visit(at, entry))
            .with_context(|| format!("segment {}", path.display()))?;
        if let Some(listed) = listed {
            ensure!(
                segment.info() == *listed,
                "{} does not match {MANIFEST}: on disk {:?}, listed {listed:?}",
                path.display(),
                segment.info()
            );
        }
    }
    Ok(())
}
