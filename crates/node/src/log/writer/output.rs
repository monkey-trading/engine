//! The open segment's file and the entries appended to it but not yet written. Appending never
//! reaches the file: a filesystem may hold a write to a file while a sync of it runs, so with the
//! log-sync thread (`pipeline_log_syncs`) the sequencer's thread hands the entries to that thread,
//! which writes them in order before it syncs, and the sequencer's thread makes no call on the
//! file. Without the thread, the writer writes them itself before its own sync. A writer dropped
//! with no job holding entries writes what is pending, unsynced, as a buffered file would on drop.

use std::fs::File;
use std::io::Write;
use std::sync::Arc;

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
