//! The open segment's file and the entries appended to it but not yet written. Appending never
//! reaches the file: a filesystem may hold a write to a file while a sync of it runs, so with the
//! log-sync thread (`pipeline_log_syncs`) the sequencer's thread hands the entries to that thread,
//! which writes them in order before it syncs, and the sequencer's thread makes no call on the
//! file. Without the thread, the writer writes them itself before its own sync. A writer dropped
//! with no job holding entries writes what is pending, unsynced, as a buffered file would on drop.

use std::fs::File;
use std::io::Write;
use std::sync::Arc;

use anyhow::{Context, ensure};

use crate::log::LogWriter;

/// The open segment's file, shared with the sync jobs that write and sync it, and its unwritten
/// entries as JSON lines.
pub(crate) struct Output {
    file: Arc<File>,
    pending: Vec<u8>,
}

impl Output {
    /// The output of `file`, with nothing pending.
    pub(crate) fn new(file: File) -> Self {
        Self { file: Arc::new(file), pending: Vec::new() }
    }

    /// The segment's file.
    pub(crate) fn file(&self) -> &Arc<File> {
        &self.file
    }

    /// Adds one encoded entry after those pending; nothing reaches the file.
    pub(crate) fn push(&mut self, line: &[u8]) {
        self.pending.extend_from_slice(line);
    }

    /// Takes the pending entries, for a sync job to write.
    pub(crate) fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    /// Writes the pending entries to the file on the caller's thread.
    /// Error cases:
    /// - the write fails, leaving part of them in the file: the writer must stop
    pub(crate) fn write_pending(&mut self) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            (&*self.file).write_all(&self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }
}

impl LogWriter {
    /// Writes on this thread the entries no job has taken, for an inline flush, sync or seal: first
    /// the tails of segments rotation completed whose seals wait for the next job, then the open
    /// segment's, so each file holds its entries in order.
    /// Error cases:
    /// - a sync job still holds entries or a seal: its writes would land after these, so the
    ///   pipeline must be settled first
    /// - a write fails
    pub(super) fn write_pending(&mut self) -> anyhow::Result<()> {
        ensure!(self.handed == 0, "an inline log write while {} sync jobs hold unwritten entries", self.handed);
        for seal in &self.pending_seals {
            seal.write_tail()?;
        }
        if let Some((_, out)) = self.open.as_mut() {
            out.write_pending().context("writing log")?;
        }
        Ok(())
    }
}

impl Drop for LogWriter {
    /// Writes the pending entries, unsynced, as a buffered file does on drop, unless a sync job
    /// still holds earlier ones: written now, these would land ahead of them.
    fn drop(&mut self) {
        if self.handed == 0 {
            let _ = self.write_pending();
        }
    }
}
