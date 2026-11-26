//! A scan of a segmented log for its entries that are not commands, its segments read several at once, one
//! per thread, and only those entries parsed: for a pass that wants only the log's checkpoints and
//! withdrawals, as a custody audit's search for the checkpoints to ask the vault about. Of what a whole read
//! checks it checks only that a completed segment is as long as its listing and ends with a whole line, so a
//! damaged file is named as such; the replay that follows reads the log in full.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context;

use crate::log::local::{first_read, pair};
use crate::log::segments::list_segments;
use crate::log::{LogEntry, MANIFEST, Manifest};

/// How every command's line begins as the log writes it. A line that begins so is a command's or no entry
/// at all, since its tag cannot appear twice; any other line is parsed.
pub(super) const COMMAND_LINE: &[u8] = br#"{"entry":"command","#;

/// What `keep` makes of each entry of the log in `dir` that is not a command, as each is read, in log order, its
/// segments read on `jobs` threads; `keep` reduces an entry to what its caller needs, so the scan holds no more.
/// The last segment's last line with no break, a write under way, is left out, as a whole read leaves it out.
/// The scan stops at the first segment that fails.
/// Error cases:
/// - the directory or the manifest cannot be read, its listings do not pair with the files by name
///   (`local::pair`), the log's oldest segments are not here (`NotLocal`), or a segment cannot be read
/// - a segment the manifest lists is of another length than its listing, or ends with no break
/// - a line that does not begin as a command's is not a valid entry
pub(crate) fn uncommanded<T: Send>(
    dir: &Path,
    jobs: NonZeroUsize,
    keep: impl Fn(LogEntry) -> Option<T> + Sync,
) -> anyhow::Result<Vec<T>> {
    // Listed before the manifest is read: a listing past the files is of a segment completed since.
    let segments = list_segments(dir)?;
    let (mut manifest, _) = Manifest::read(dir)?;
    let segments = pair(dir, &mut manifest, segments, true)?;
    first_read(&manifest, &segments, None)?;
    let (files, listings): (Vec<PathBuf>, Vec<Option<u64>>) =
        segments.into_iter().map(|(_, path, listed)| (path, listed.map(|listing| listing.bytes))).unzip();
    let listed = |at: usize| listings[at];
    let (next, failed) = (AtomicUsize::new(0), AtomicUsize::new(usize::MAX));
    let found: Vec<Mutex<Option<anyhow::Result<Vec<T>>>>> = files.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..jobs.get().min(files.len()) {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(at).filter(|_| at < failed.load(Ordering::Relaxed)) else { return };
                    let scanned = segment(path, listed(at), &keep);
                    if scanned.is_err() {
                        failed.fetch_min(at, Ordering::Relaxed);
                    }
                    *found[at].lock().expect("no thread panics holding a segment's entries") = Some(scanned);
                }
            });
        }
    });
    // Every segment before the first that failed is scanned, and that one gives its fault.
    let mut entries = Vec::new();
    for slot in found {
        let scanned = slot.into_inner().expect("no thread panics holding a segment's entries");
        entries.extend(scanned.expect("every segment up to the first that failed is scanned")?);
    }
    Ok(entries)
}

/// What `keep` makes of the entries of the segment at `path` that are not commands, in order, read a line at a
/// time; `listed` is its length in the manifest, for a completed segment.
/// Error cases:
/// - the file cannot be read, is of another length than `listed`, or, listed, ends with no break
/// - a line that does not begin as a command's is not a valid entry
fn segment<T>(path: &Path, listed: Option<u64>, keep: &impl Fn(LogEntry) -> Option<T>) -> anyhow::Result<Vec<T>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    if let Some(bytes) = listed {
        let held = file.metadata().with_context(|| format!("reading {}", path.display()))?.len();
        anyhow::ensure!(held == bytes, "{} differs from {MANIFEST}: {held} bytes, listed {bytes}", path.display());
    }
    let mut input = BufReader::new(file);
    let (mut entries, mut line, mut n) = (Vec::new(), Vec::new(), 0usize);
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line).with_context(|| format!("reading {}", path.display()))? == 0 {
            return Ok(entries);
        }
        n += 1;
        // A last line with no break is a write under way, which a completed segment holds none of.
        let Some(whole) = line.strip_suffix(b"\n") else {
            anyhow::ensure!(listed.is_none(), "{} ends mid-line", path.display());
            return Ok(entries);
        };
        if whole.starts_with(COMMAND_LINE) || whole.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let entry = serde_json::from_slice(whole).with_context(|| format!("{} line {n}", path.display()))?;
        // A command written otherwise than the log writes it is parsed, and left out all the same.
        if !matches!(entry, LogEntry::Command(_)) {
            entries.extend(keep(entry));
        }
    }
}
