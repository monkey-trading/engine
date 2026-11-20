//! Takeover truncation of a standby's log copy: dropping everything after the checkpoint a
//! promotion resumes from, which the outgoing operator published but the vault never accepted, and
//! the read of the log's last checkpoint a start decides that cut by.
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

use crate::log::segments::{Segment, list_segments};
use crate::log::{Checkpoint, LogEntry, MANIFEST, Manifest, SegmentInfo, parse_line};

/// The last checkpoint the log in `dir` holds, read from its newest segment back to the first that
/// holds one, so a start's takeover cut reads about one checkpoint interval of the log, however long
/// it is. Each segment read passes `SegmentedLog::open`'s checks for it: a listed one matches its
/// listing before any of it is read, only the last may be unlisted, and its torn last line is left
/// out. `None` when no segment holds a checkpoint.
/// Error cases:
/// - the directory, the manifest or a segment read cannot be read, or a segment read fails a check
pub(crate) fn last_checkpoint(dir: &Path) -> anyhow::Result<Option<Checkpoint>> {
    let (files, listings) = (list_segments(dir)?, Manifest::load(dir)?.segments);
    ensure!(
        listings.len() <= files.len(),
        "{MANIFEST} lists {} segments but {} are in {}",
        listings.len(),
        files.len(),
        dir.display()
    );
    for (index, (first_seq, path)) in files.iter().enumerate().rev() {
        let listed = listings.get(index);
        ensure!(
            listed.is_some() || index + 1 == files.len(),
            "{} is followed by another segment but is not in {MANIFEST}",
            path.display()
        );
        let segment = Segment::read(*first_seq, path, listed, &mut None, &mut |_| Ok(()))
            .with_context(|| format!("segment {}", path.display()))?;
        ensure!(listed.is_none_or(|listed| segment.info() == *listed), "{} does not match {MANIFEST}", path.display());
        if let Some(last) = segment.checkpoints.last() {
            return Ok(Some(last.checkpoint.clone()));
        }
    }
    Ok(None)
}

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
        // Durably install the replacement manifest before anything is deleted: the staged file
        // and its rename must survive a crash, or the repair marker cannot reconstruct it.
        let staged = dir.join(format!("{MANIFEST}.staging"));
        fs::write(&staged, text).with_context(|| format!("writing {}", staged.display()))?;
        let file = fs::File::open(&staged)?;
        file.sync_all().with_context(|| format!("syncing {}", staged.display()))?;
        fs::rename(&staged, dir.join(MANIFEST)).with_context(|| format!("linking {MANIFEST}"))?;
        sync_dir(dir)?;
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
    sync_dir(dir)
}

/// Forces the directory's entries to storage, so renames and removals survive a power loss.
/// Error cases:
/// - the directory cannot be opened or synced
fn sync_dir(dir: &Path) -> anyhow::Result<()> {
    fs::File::open(dir)
        .with_context(|| format!("opening {}", dir.display()))?
        .sync_all()
        .with_context(|| format!("syncing {}", dir.display()))
}
