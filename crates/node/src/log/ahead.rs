//! A log read on a thread of its own, ahead of the caller that takes its entries: the thread reads,
//! checks and parses the log exactly as an in-place read does, and the caller's `visit` takes each
//! entry in log order, on the caller's thread. An entry `visit` refuses ends the read with `visit`'s
//! error, in the context an in-place read gives it (the segment or file the entry is in), and an entry
//! the read refuses ends it with the read's error once `visit` has taken every entry before it, so
//! either error is the one an in-place read gives, at the same entry. At most `AHEAD` entries wait
//! between the two threads.

use std::panic;
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::thread;

use anyhow::Context;
use tracing::warn;

use crate::log::segments::list_segments;
use crate::log::{LogEntry, SegmentedLog, Stop, read_file};

/// How many entries the reading thread may hold that the caller has not taken.
const AHEAD: usize = 1024;

/// A read that hands each entry, with the index of the file it is in, to the visitor it is given.
type Read<T> = Box<dyn FnOnce(&mut dyn FnMut(usize, LogEntry) -> anyhow::Result<()>) -> anyhow::Result<T> + Send>;

impl SegmentedLog {
    /// `SegmentedLog::open`, reading, checking and parsing the segments on a thread of its own while
    /// `visit` takes each entry on this one.
    /// Error cases:
    /// - as `SegmentedLog::open`
    pub fn open_ahead(dir: &Path, visit: impl FnMut(LogEntry) -> anyhow::Result<()>) -> anyhow::Result<Self> {
        let files = list_segments(dir)?;
        let paths: Vec<_> = files.iter().map(|(_, path)| path.clone()).collect();
        let dir = dir.to_owned();
        let read: Read<Self> = Box::new(move |each| SegmentedLog::open_listed(&dir, files, false, each));
        // `open_listed` gives an error from reading a segment this context.
        ahead(read, visit, |at| format!("segment {}", paths[at].display()))
    }
}

/// `read_file`, reading and parsing the file on a thread of its own while `visit` takes each entry on
/// this one.
/// Error cases:
/// - as `read_file`
pub fn read_file_ahead(path: &Path, visit: impl FnMut(LogEntry) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let file = path.to_owned();
    let read: Read<()> = Box::new(move |each| read_file(&file, |entry| each(0, entry)));
    // `read_file` gives an error from reading the file this context.
    ahead(read, visit, |_| format!("log {}", path.display()))
}

/// `visit`, taking an entry and the index of the file it is in, which it leaves aside.
pub(super) fn by_entry<R>(mut visit: impl FnMut(LogEntry) -> R) -> impl FnMut(usize, LogEntry) -> R {
    move |_, entry| visit(entry)
}

/// Runs `read` on a thread of its own, handing its entries to `visit` here in order, and returns what
/// `read` returns, or `visit`'s error in the context `place` gives the file of its entry, which comes
/// at an earlier entry than any error `read` gives after it. Reads in place when the thread cannot
/// start.
/// Error cases:
/// - `visit` refuses an entry, or `read` fails
fn ahead<T: Send + 'static>(
    read: Read<T>,
    mut visit: impl FnMut(LogEntry) -> anyhow::Result<()>,
    place: impl Fn(usize) -> String,
) -> anyhow::Result<T> {
    let (entries, taken) = sync_channel(AHEAD);
    let (start, reading) = sync_channel(1);
    let reader = thread::Builder::new().name("log-reader".into()).spawn(move || {
        // Waits for the caller to hold the read, so a thread it could not keep reads nothing.
        let Ok(read) = reading.recv() else { return None };
        let read: Read<T> = read;
        Some(read(&mut |at, entry| entries.send((at, entry)).map_err(|_| Stop.into())))
    });
    let reader = match reader {
        Ok(reader) => reader,
        Err(e) => {
            warn!("reading the log in place: its reader thread did not start: {e}");
            return read(&mut by_entry(visit));
        }
    };
    start.send(read).expect("the reader waits for its read");
    let visited = taken.iter().try_for_each(|(at, entry)| visit(entry).with_context(|| place(at)));
    // The reader's next send fails once the caller stops taking entries, and its read ends there.
    drop(taken);
    let read = reader.join().unwrap_or_else(|panicked| panic::resume_unwind(panicked));
    visited?;
    read.expect("the reader read once it held the read")
}

#[cfg(test)]
mod tests;
