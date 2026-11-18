//! Recovered appends use the same descriptor whose complete actual prefix was checked.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::log::segments::{Segment, segment_name};

/// Refuses late host changes before any recovered log mutation or writable catalog preparation. A
/// last line the read left out as torn is dropped first, so the next entry starts on its own line.
/// Error cases:
/// - opening, exact length, actual prefix hash or same-descriptor IO validation fails
/// - the torn line cannot be dropped
pub(super) fn open_active(directory: &Path, segment: &Segment) -> Result<std::fs::File> {
    let path = directory.join(segment_name(segment.first_seq));
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening recovered active log {}", path.display()))?;
    if segment.torn > 0 {
        let read = segment.bytes + segment.torn;
        ensure!(file.metadata()?.len() == read, "recovered active log length changed before append");
        file.set_len(segment.bytes).with_context(|| format!("dropping the torn last line of {}", path.display()))?;
        warn!(segment = %path.display(), bytes = segment.torn, "dropped a log line a crash cut short: it was never synced");
    }
    ensure!(file.metadata()?.len() == segment.bytes, "recovered active log length changed before append");
    let mut input = file.try_clone()?;
    input.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut remaining = segment.bytes;
    let mut buffer = [0_u8; 8192];
    while remaining > 0 {
        let length = usize::try_from(remaining.min(buffer.len() as u64))?;
        let read = input.read(&mut buffer[..length])?;
        ensure!(read > 0, "recovered active log was truncated before append");
        hash.update(&buffer[..read]);
        remaining -= read as u64;
    }
    ensure!(hex::encode(hash.finalize()) == segment.info().sha256, "recovered active log bytes changed before append");
    ensure!(file.metadata()?.len() == segment.bytes, "recovered active log length changed during append validation");
    Ok(file)
}
