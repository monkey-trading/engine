//! A scan of a segmented log for its entries that are not commands, every segment read at once and only
//! those entries parsed: for a pass that wants only the log's checkpoints and withdrawals, as a custody
//! audit's search for the checkpoints to ask the vault about. It checks none of what a whole read checks,
//! so the replay that follows reads the log in full.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context;

use crate::log::LogEntry;
use crate::log::segments::list_segments;

/// How every command's line begins as the log writes it. A line that begins so is a command's or no entry
/// at all, since its tag cannot appear twice; any other line is parsed.
pub(super) const COMMAND_LINE: &[u8] = br#"{"entry":"command","#;

/// The entries of the log in `dir` that are not commands, in log order, its segments read on `jobs` threads.
/// Each segment's last line with no break is left out: in the last segment a write under way, which a whole
/// read leaves out too, and in a completed one a fault the replay that follows refuses.
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
            slot.into_inner()
                .expect("no thread panics holding a segment's entries")
                .expect("every segment is scanned")?,
        );
    }
    Ok(entries)
}

/// The entries of the segment at `path` that are not commands, in order, read a line at a time.
/// Error cases:
/// - the file cannot be read, or a line that does not begin as a command's is not a valid entry
fn segment(path: &Path) -> anyhow::Result<Vec<LogEntry>> {
    let mut input = BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let (mut entries, mut line, mut n) = (Vec::new(), Vec::new(), 0usize);
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line).with_context(|| format!("reading {}", path.display()))? == 0 {
            return Ok(entries);
        }
        n += 1;
        // A last line with no break is a write under way.
        let Some(whole) = line.strip_suffix(b"\n") else { return Ok(entries) };
        if whole.starts_with(COMMAND_LINE) || whole.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let entry = serde_json::from_slice(whole).with_context(|| format!("{} line {n}", path.display()))?;
        // A command written otherwise than the log writes it is parsed, and left out all the same.
        if !matches!(entry, LogEntry::Command(_)) {
            entries.push(entry);
        }
    }
}
