//! A scan of a segmented log for its entries that are not commands, every segment read at once and only
//! those entries parsed: for a pass that wants only the log's checkpoints and withdrawals, as a custody
//! audit's search for the checkpoints to ask the vault about. It checks none of what a whole read checks,
//! so the replay that follows reads the log in full.

use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context;

use crate::log::LogEntry;
use crate::log::segments::list_segments;

/// How every command's line begins as the log writes it. A line that begins so is a command's or no entry
/// at all, since its tag cannot appear twice; any other line is parsed.
const COMMAND_LINE: &[u8] = br#"{"entry":"command","#;

/// The entries of the log in `dir` that are not commands, in log order, its segments read on `jobs` threads.
/// A last line with no break, a write under way, is left out, as a whole read leaves it out.
/// Error cases:
/// - the directory cannot be listed, or a segment cannot be read
/// - a line that does not begin as a command's is not a valid entry
pub(crate) fn uncommanded(dir: &Path, jobs: NonZeroUsize) -> anyhow::Result<Vec<LogEntry>> {
    let files: Vec<PathBuf> = list_segments(dir)?.into_iter().map(|(_, path)| path).collect();
    let next = AtomicUsize::new(0);
    let found: Vec<Mutex<Option<anyhow::Result<Vec<LogEntry>>>>> = files.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..jobs.get().min(files.len()) {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(at) else { return };
                    *found[at].lock().expect("no thread panics holding a segment's entries") = Some(segment(path));
                }
            });
        }
    });
    let mut entries = Vec::new();
    for slot in found {
        entries.extend(
            slot.into_inner().expect("no thread panics holding a segment's entries").unwrap_or(Ok(Vec::new()))?,
        );
    }
    Ok(entries)
}

/// The entries of the segment at `path` that are not commands, in order.
/// Error cases:
/// - the file cannot be read, or a line that does not begin as a command's is not a valid entry
fn segment(path: &Path) -> anyhow::Result<Vec<LogEntry>> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    // What follows the last break is a write under way, or nothing.
    lines.pop();
    lines
        .into_iter()
        .enumerate()
        .filter(|(_, line)| !line.starts_with(COMMAND_LINE) && !line.iter().all(u8::is_ascii_whitespace))
        .map(|(n, line)| serde_json::from_slice(line).with_context(|| format!("{} line {}", path.display(), n + 1)))
        .collect()
}
