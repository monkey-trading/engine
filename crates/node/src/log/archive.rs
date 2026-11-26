//! Checked completed-segment bytes for the public archive response, and the writer's active prefix for the
//! archive's uploads.

use std::io::{self, Read};
use std::path::Path;

use crate::log::checked::{self, CheckedChunks};
use crate::log::{ActivePrefix, Manifest, SegmentInfo};

pub(crate) struct ArchiveReader(CheckedChunks);

impl Read for ArchiveReader {
    /// Exposes bytes from an already verified private chunk buffer.
    /// Error cases:
    /// - a needed chunk changed or cannot be read
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.0.read(out)
    }
}

/// Opens only the exact authenticated completed descriptor, checking each later reread.
/// Error cases:
/// - the file is missing, its length/hash differs, or it changes between verification and use
pub(crate) fn open_segment_archive(dir: &Path, listed: &SegmentInfo) -> anyhow::Result<ArchiveReader> {
    let path = dir.join(&listed.file);
    Ok(ArchiveReader(CheckedChunks::open(&path, checked::check(&path, listed)?, 0)?))
}

/// Opens the prefix of the open segment that `view`'s writer has flushed and published, checking each byte read
/// against the prefix's hash, however much the file has grown since; `None` when the view publishes no prefix.
/// Error cases:
/// - the view is not a writer's, or its prefix contradicts its completed segments
/// - the file is missing, or its prefix's length or hash differs, or it changes between verification and use
pub(crate) fn open_active_archive(
    dir: &Path,
    view: &Manifest,
) -> anyhow::Result<Option<(ActivePrefix, ArchiveReader)>> {
    view.ensure_verified()?;
    let Some(active) = view.active_prefix()? else { return Ok(None) };
    let path = dir.join(&active.info.file);
    let (hashes, _) = checked::extend(&path, &active.info, false, None)?;
    Ok(Some((active, ArchiveReader(CheckedChunks::open(&path, hashes, 0)?))))
}
