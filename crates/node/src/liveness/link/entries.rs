//! The entries a member answers an `Entries` question with: the ring of its copy's latest entries
//! while that joins the asker's tip, else a page of the copy's durable log, which also serves an asker
//! whose copy is empty.

use std::path::PathBuf;

use tokio::sync::watch;
use tracing::warn;

use super::{Copy, ENTRIES_BYTES};
use crate::log::{LogEntry, LogReader, Manifest, Offset};

/// A member's copy on disk: its directory, and the manifest its writer publishes, which bounds a read
/// to what the copy has flushed.
#[derive(Clone, Debug)]
pub(super) struct DurableLog {
    dir: PathBuf,
    manifest: watch::Receiver<Manifest>,
}

impl Copy {
    /// Records the copy's log on disk, which answers an entries question the ring does not.
    pub(crate) fn logged(&self, dir: PathBuf, manifest: watch::Receiver<Manifest>) {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).durable_log =
            Some(DurableLog { dir, manifest });
    }

    /// The copy's retained entries after the entry at `after`, or from its first entry for `None`,
    /// oldest first, up to `ENTRIES_BYTES` of JSON. Empty unless the copy retains the entry at `after`
    /// or the one right after it: a gap in what is retained would leave the asker with entries it
    /// cannot chain.
    pub(crate) fn entries_after(&self, after: Option<Offset>) -> Vec<(Offset, LogEntry)> {
        let state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = match after {
            Some(after) => state.recent.iter().position(|(offset, _)| *offset > after),
            None => (!state.recent.is_empty()).then_some(0),
        };
        let Some(start) = start else { return Vec::new() };
        // The next seq's command joins an asker's entry only when no entry between them can be missing,
        // which a ring cannot tell: with the log on disk, the page is read from it instead.
        let joined = match start {
            0 if state.durable_log.is_some() => follows_exactly(after, state.recent[0].0),
            0 => follows(after, state.recent[0].0),
            _ => Some(state.recent[start - 1].0) == after,
        };
        if !joined {
            return Vec::new();
        }
        bounded(state.recent.iter().skip(start).cloned())
    }

    /// `entries_after`, else the same page read from the copy's durable log: a copy that trails the
    /// asker by more than its ring, or an asker holding nothing, is served from disk.
    pub(crate) async fn entries(&self, after: Option<Offset>) -> Vec<(Offset, LogEntry)> {
        let held = self.entries_after(after);
        let log = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).durable_log.clone();
        let Some(log) = log.filter(|_| held.is_empty()) else { return held };
        match tokio::task::spawn_blocking(move || log.page(after)).await {
            Ok(Ok(page)) => page,
            Ok(Err(e)) => {
                warn!(?after, "reading the copy's log for an entries question failed: {e:#}");
                Vec::new()
            }
            Err(e) => {
                warn!(?after, "the copy's log read did not finish: {e}");
                Vec::new()
            }
        }
    }
}

impl DurableLog {
    /// The page `entries_after` gives, read from disk.
    /// Error cases:
    /// - the log cannot be opened or read (see `LogReader`)
    fn page(&self, after: Option<Offset>) -> anyhow::Result<Vec<(Offset, LogEntry)>> {
        let mut reader = LogReader::open(&self.dir, after.unwrap_or(Offset::start_of(0)), self.manifest.clone())?;
        let (mut entries, mut bytes) = (Vec::new(), 0);
        while let Some(next) = reader.next_entry()? {
            if after.is_some_and(|after| next.0 <= after) {
                continue;
            }
            if entries.is_empty() && !follows(after, next.0) {
                return Ok(Vec::new());
            }
            bytes += size(&next.1);
            if bytes > ENTRIES_BYTES {
                break;
            }
            entries.push(next);
        }
        Ok(entries)
    }
}

/// Whether `first` is the entry right after `after`, or the first of the next seq, or for `None` the
/// log's first. `after` is any peer's: the offsets right after it saturate rather than overflow.
fn follows(after: Option<Offset>, first: Offset) -> bool {
    match after {
        Some(after) => [after.next(), Offset::start_of(after.seq.saturating_add(1))].contains(&first),
        None => first == Offset::start_of(0),
    }
}

/// Whether `first` is the entry right after `after` within its seq, or the log's first for `None`.
fn follows_exactly(after: Option<Offset>, first: Offset) -> bool {
    after.map_or(first == Offset::start_of(0), |after| first == after.next())
}

/// The leading entries of `entries` that fit `ENTRIES_BYTES` of JSON.
fn bounded(entries: impl Iterator<Item = (Offset, LogEntry)>) -> Vec<(Offset, LogEntry)> {
    let mut bytes = 0;
    entries
        .take_while(|(_, entry)| {
            bytes += size(entry);
            bytes <= ENTRIES_BYTES
        })
        .collect()
}

/// The bytes `entry` takes in an answer.
fn size(entry: &LogEntry) -> usize {
    serde_json::to_vec(entry).map_or(usize::MAX / 2, |encoded| encoded.len())
}
