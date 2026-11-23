//! Writes of the derived stores, flushed in small steps. A store that writes a large file at once (a recovery
//! cache, a daily snapshot, a financing rollover's rows) leaves the kernel a dirty backlog of that size, and
//! the flush that follows holds every `fdatasync` of the log on the same disk behind it, and every order and
//! stream frame waiting on the log's sync with it. `Paced` syncs the file's data every `STEP` bytes as they
//! are written, so what is dirty never passes a step. It changes when bytes are flushed, never which are
//! written.

use std::borrow::Borrow;
use std::fs::File;
use std::io::{self, Write};

/// The bytes written between syncs: a flush of a step takes a few milliseconds on a disk of the production
/// class, where one of the whole file held the log's syncs for over a second.
pub(crate) const STEP: usize = 4 << 20;

/// A file's writer that syncs its data every `STEP` bytes. The caller still syncs the file once at the end,
/// which makes the file and its length durable.
pub(crate) struct Paced<F> {
    inner: F,
    since_sync: usize,
}

impl<F: Write + Borrow<File>> Paced<F> {
    pub(crate) fn new(inner: F) -> Self {
        Self { inner, since_sync: 0 }
    }

    /// The file written.
    pub(crate) fn get_ref(&self) -> &File {
        self.inner.borrow()
    }
}

impl<F: Write + Borrow<File>> Write for Paced<F> {
    /// Writes at most up to the step's end, so a large buffer is written in steps and synced between them.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let room = STEP - self.since_sync;
        let written = self.inner.write(&buf[..buf.len().min(room)])?;
        self.since_sync += written;
        if self.since_sync >= STEP {
            self.inner.flush()?;
            self.inner.borrow().sync_data()?;
            self.since_sync = 0;
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
#[path = "paced_tests.rs"]
mod tests;
