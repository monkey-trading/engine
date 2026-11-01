//! Takeover truncation of a standby's log copy: dropping everything after the checkpoint a
//! promotion resumes from, which the outgoing operator published but the vault never accepted.
//!
//! The surgery is ordered so any crash leaves a state `SegmentedLog::open` accepts and the next
//! start can resume: the manifest is rewritten first (dropping the listings that would reference
//! a trimmed or deleted file), then later segments are deleted newest first, then the target
//! segment is trimmed. A crash before the trim leaves the segment unlisted with its tail intact,
//! which the next start truncates again; a crash between deletions leaves the newest remaining
//! file unlisted and last, which opens as the open segment.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, bail, ensure};

use crate::log::{LogEntry, MANIFEST, SegmentInfo, parse_line, segments::list_segments};

/// Truncates the log in `dir` so its last entry is the checkpoint at `seq` with `root`, dropping
/// every later entry. The checkpoint's segment keeps its name and, when the checkpoint ends it,
/// its listing; otherwise the segment loses its listing and stands as the open one, exactly as if
/// its writer had stopped there. Later segment files and their listings are removed. The caller
/// must hold no open handles on the log: it drops its writer first.
/// Error cases:
/// - the log cannot be read, or holds no checkpoint at `seq` with `root`
/// - a segment or the manifest cannot be rewritten
pub(crate) fn truncate_to_checkpoint(dir: &Path, seq: u64, root: &str) -> anyhow::Result<()> {
    let files = list_segments(dir)?;
    let mut target: Option<(usize, u64)> = None;
    for (index, (_, path)) in files.iter().enumerate() {
        let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut end = 0u64;
        let mut n = 0usize;
        while reader.read_until(b'\n', &mut line).context("reading segment")? > 0 {
            ensure!(line.ends_with(b"\n"), "{} ends mid-line", path.display());
            end += line.len() as u64;
            if let Some(LogEntry::Checkpoint(checkpoint)) = parse_line(n, &line)?
                && checkpoint.seq == seq
                && checkpoint.root == root
            {
                target = Some((index, end));
                break;
            }
            n += 1;
            line.clear();
        }
        if target.is_some() {
            break;
        }
    }
    let Some((index, end)) = target else {
        bail!("the log in {} holds no checkpoint at seq {seq} with root {root}", dir.display());
    };
    let (target_first, target_path) = &files[index];
    let file_len = fs::metadata(target_path).with_context(|| format!("reading {}", target_path.display()))?.len();
    let trimmed = end != file_len;
    // First make the manifest consistent with the intended end state, so a crash from here on
    // leaves only states that open: the target's listing goes only when the segment is trimmed
    // (its bytes then differ from the listing), and every later listing goes with its file.
    let manifest = fs::read_to_string(dir.join(MANIFEST)).unwrap_or_default();
    let listings: Vec<SegmentInfo> = manifest
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("parsing a manifest line"))
        .collect::<anyhow::Result<_>>()?;
    let kept: Vec<&SegmentInfo> = listings
        .iter()
        .filter(|listed| listed.first_seq < *target_first || (!trimmed && listed.first_seq == *target_first))
        .collect();
    if kept.len() != listings.len() {
        let mut text = String::new();
        for listed in kept {
            text.push_str(&serde_json::to_string(listed).context("encoding a manifest line")?);
            text.push('\n');
        }
        fs::write(dir.join(MANIFEST), text).with_context(|| format!("writing {MANIFEST}"))?;
    }
    // Newest first: each deletion leaves the remaining newest file last, and unlisted files may
    // stand last. A listed file is never deleted. A deletion another run already made is not an
    // error: the surgery is idempotent, so a restart can re-run it after a crash.
    for (_, path) in files.iter().skip(index + 1).rev() {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    if trimmed {
        let file = fs::OpenOptions::new().write(true).open(target_path)?;
        file.set_len(end).with_context(|| format!("truncating {}", target_path.display()))?;
        file.sync_all().with_context(|| format!("syncing {}", target_path.display()))?;
    }
    fs::File::open(dir).with_context(|| format!("opening {}", dir.display()))?.sync_all().ok();
    Ok(())
}
