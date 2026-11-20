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
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, bail, ensure};

use crate::log::segments::{Segment, list_segments, segment_name, segment_seq};
use crate::log::{Checkpoint, LogEntry, MANIFEST, SegmentInfo, parse_line};

/// The manifest's last bytes read first to find its newest listings, a few hundred of them; each later
/// read takes four times as many.
const MANIFEST_TAIL_BYTES: u64 = 64 * 1024;

/// The last checkpoint the log in `dir` holds, read from its newest segment back to the first that
/// holds one, so a start's takeover cut reads about one checkpoint interval of the log, however long
/// it is. The segments are found from the manifest's end: the newest listings, and before them the
/// segment after the newest, which the writer has not listed yet, when it is there. No segment may
/// follow the one the writer appends to, as a whole read requires; the directory's names alone show
/// that. A listed segment matches its listing before any of it is read, as a whole read checks it,
/// and the unlisted one's torn last line is left out, as a whole read leaves it out. `None` when no
/// segment holds a checkpoint.
/// Error cases:
/// - the manifest or a segment cannot be read, a manifest line is not a listing, a segment follows the
///   one the writer appends to, or a segment read differs from its listing, holds an invalid entry or
///   does not start with its command
pub(crate) fn last_checkpoint(dir: &Path) -> anyhow::Result<Option<Checkpoint>> {
    let mut window = MANIFEST_TAIL_BYTES;
    let (mut listings, mut whole) = manifest_tail(dir, window)?;
    while listings.is_empty() && !whole {
        window = window.saturating_mul(4);
        (listings, whole) = manifest_tail(dir, window)?;
    }
    let next = match listings.first() {
        Some(listed) => listed.last_seq.checked_add(1).context("the newest listing's last seq overflows")?,
        None => 0,
    };
    let open = dir.join(segment_name(next));
    let open_exists = open.try_exists().with_context(|| format!("probing {}", open.display()))?;
    let appended = if open_exists { Some(next) } else { listings.first().map(|listed| listed.first_seq) };
    let newest = newest_segment(dir)?;
    ensure!(
        newest == appended,
        "segment {newest:?} in {} follows the one the writer appends to, {appended:?}",
        dir.display()
    );
    if open_exists && let Some(checkpoint) = last_in(next, &open, None)? {
        return Ok(Some(checkpoint));
    }
    let mut below = next;
    loop {
        let unread = below;
        for listed in listings.iter().filter(|listed| listed.first_seq < unread) {
            if let Some(checkpoint) =
                last_in(listed.first_seq, &dir.join(segment_name(listed.first_seq)), Some(listed))?
            {
                return Ok(Some(checkpoint));
            }
            below = listed.first_seq;
        }
        if whole {
            return Ok(None);
        }
        window = window.saturating_mul(4);
        (listings, whole) = manifest_tail(dir, window)?;
    }
}

/// The seq the newest segment file in `dir` is named for, from the directory's names alone; `None` for
/// a directory with none, or none at all.
/// Error cases:
/// - the directory exists but cannot be listed
fn newest_segment(dir: &Path) -> anyhow::Result<Option<u64>> {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    let mut newest = None;
    for item in listing {
        let name = item.with_context(|| format!("listing {}", dir.display()))?.file_name();
        newest = newest.max(name.to_str().and_then(segment_seq));
    }
    Ok(newest)
}

/// The last checkpoint in the segment named for `first_seq` at `path`, read as `SegmentedLog::open`
/// reads it: one `listed` matches its listing before any of it is read, and an unlisted one's torn
/// last line is left out.
/// Error cases:
/// - the segment cannot be read, differs from its listing, holds an invalid entry, or does not start
///   with the command for `first_seq`
fn last_in(first_seq: u64, path: &Path, listed: Option<&SegmentInfo>) -> anyhow::Result<Option<Checkpoint>> {
    let segment = Segment::read(first_seq, path, listed, &mut None, &mut |_| Ok(()))
        .with_context(|| format!("segment {}", path.display()))?;
    if let Some(listed) = listed {
        ensure!(
            segment.info() == *listed,
            "{} does not match {MANIFEST}: on disk {:?}, listed {listed:?}",
            path.display(),
            segment.info()
        );
    }
    Ok(segment.checkpoints.last().map(|locator| locator.checkpoint.clone()))
}

/// The listings in the last `window` bytes of the manifest in `dir`, newest first, and whether those
/// bytes are the whole manifest. A line the window cuts into is left out, and so is a last line with
/// no line break, which a crash while listing leaves, as `Manifest::read` leaves it out. A missing
/// manifest lists nothing.
/// Error cases:
/// - the manifest exists but cannot be read, or a whole line in the window is not a valid listing
fn manifest_tail(dir: &Path, window: u64) -> anyhow::Result<(Vec<SegmentInfo>, bool)> {
    let path = dir.join(MANIFEST);
    let mut file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), true)),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let start = file.metadata().with_context(|| format!("reading {}", path.display()))?.len().saturating_sub(window);
    file.seek(SeekFrom::Start(start)).with_context(|| format!("reading {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(window).read_to_end(&mut bytes).with_context(|| format!("reading {}", path.display()))?;
    let lines = &bytes[..bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1)];
    // A window that starts inside the manifest starts inside a line.
    let lines = match start {
        0 => lines,
        _ => lines.iter().position(|b| *b == b'\n').map_or(&[][..], |i| &lines[i + 1..]),
    };
    let listings = lines
        .split(|b| *b == b'\n')
        .filter(|line| !line.trim_ascii().is_empty())
        .map(|line| serde_json::from_slice(line).with_context(|| format!("a {} line", path.display())))
        .collect::<anyhow::Result<Vec<SegmentInfo>>>()?;
    Ok((listings.into_iter().rev().collect(), start == 0))
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
