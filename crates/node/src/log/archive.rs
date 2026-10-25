//! Checked completed-segment bytes for the public archive response.

use std::io::{self, Read};
use std::path::Path;

use crate::log::SegmentInfo;
use crate::log::checked::{self, CheckedChunks};

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
