//! A read of a segmented log from the segment holding a given command to the log's end, for a replay of
//! one range of it: none of the segments before is read, and each completed segment read is checked
//! against its listing in the manifest before any of it reaches the visitor, and an empty unlisted last
//! segment must be named for the seq after the last entry, as a whole read checks them (`SegmentedLog::open`).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};

use crate::log::segments::{Segment, list_segments};
use crate::log::{LogEntry, MANIFEST, Manifest, Offset};

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
    let (mut last, mut open) = (None, None);
    for (at, (first_seq, path)) in files.iter().enumerate().skip(first) {
        let listed = manifest.segments.get(at);
        if listed.is_none() && at + 1 < files.len() {
            bail!("{} is followed by another segment but is not in {MANIFEST}", path.display());
        }
        let segment = Segment::read(*first_seq, path, listed, &mut last, &mut |entry| visit(at, entry))
            .with_context(|| format!("segment {}", path.display()))?;
        match listed {
            Some(listed) => ensure!(
                segment.info() == *listed,
                "{} does not match {MANIFEST}: on disk {:?}, listed {listed:?}",
                path.display(),
                segment.info()
            ),
            None => open = Some(segment),
        }
    }
    // A span that read nothing starts past the log's end, where its range finds no checkpoint.
    if first == 0 || last.is_some() {
        Segment::check_open(open.as_ref(), last)?;
    }
    Ok(())
}

impl Segment {
    /// Checks the log's unlisted last segment, `open`, read after the entry at `last`: one that holds no
    /// command must be named for the seq after `last`, as every read of the log requires.
    /// Error cases:
    /// - `open` holds no command and is named for another seq
    pub(super) fn check_open(open: Option<&Segment>, last: Option<Offset>) -> anyhow::Result<()> {
        if let Some(segment) = open.filter(|segment| segment.commands == 0) {
            let next_seq = last.map_or(0, |last| last.seq + 1);
            ensure!(segment.first_seq == next_seq, "empty last segment is not named for the next seq {next_seq}");
        }
        Ok(())
    }
}
