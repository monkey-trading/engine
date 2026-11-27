//! A stream subscriber's reads of the log on disk, a batch at a time on the blocking pool.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, watch};
use tokio::task::{JoinHandle, spawn_blocking};

use crate::log::{LogReader, Manifest, Offset};
use crate::sequencer::StreamMsg;

/// Most messages read from disk per blocking call.
pub(super) const BACKFILL_BATCH: usize = 256;

/// Runs `read` on the blocking pool holding `permit` until it returns, so a subscriber that leaves
/// mid-read does not free its backfill permit while the read still runs.
pub(super) fn read_holding<T: Send + 'static>(
    permit: OwnedSemaphorePermit,
    read: impl FnOnce() -> T + Send + 'static,
) -> JoinHandle<T> {
    spawn_blocking(move || {
        let _held = permit;
        read()
    })
}

/// Up to `BACKFILL_BATCH` messages from disk, opening a reader at `from`, holding what it reads with `holding`, when
/// there is none yet, and the reader to go on with, released so it holds no segment in memory, or `None` once it has
/// read everything written.
/// Error cases:
/// - the log cannot be read (see `LogReader`)
pub(super) fn read_batch(
    dir: &Path,
    manifest: watch::Receiver<Manifest>,
    (from, holding): (Offset, bool),
    reader: Option<LogReader>,
) -> anyhow::Result<(Option<LogReader>, Vec<Arc<StreamMsg>>)> {
    let mut reader = match reader {
        Some(reader) => reader,
        None if holding => LogReader::open(dir, from, manifest)?,
        None => LogReader::open(dir, from, manifest)?.unheld(),
    };
    let mut batch = Vec::new();
    while batch.len() < BACKFILL_BATCH {
        let Some((offset, entry)) = reader.next_entry()? else {
            return Ok((None, batch));
        };
        batch.push(Arc::new(StreamMsg::new(offset, entry)));
    }
    reader.release();
    Ok((Some(reader), batch))
}
