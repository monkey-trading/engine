//! The archive's ranges: `RANGE_SEQS` sequence numbers each, built from whichever of a copy's completed segments
//! hold them. A command's entries never straddle two segments and every segment starts with a command, so a range
//! is whole once a completed segment holds its last seq, and its bytes are the same however the copy's segments
//! are cut. A checkpoint's tail is in pieces shaped as ranges, under a key named for the checkpoint, and may read the
//! writer's active prefix too: every piece before the checkpoint's range is a whole range, and the checkpoint's own
//! piece ends with its entry.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Cursor, Read};
use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use serde::Deserialize;

use crate::log::{LogEntry, Manifest, SegmentInfo, open_active_archive, open_segment_archive};

/// How many sequence numbers one object of the archive holds: 10,000, about 5.5 MB of launch flow and under a
/// megabyte compressed; six in tests, so ranges cross the test log's four-command segments.
pub(crate) const RANGE_SEQS: u64 = if cfg!(test) { 6 } else { 10_000 };

/// What a log line's entry is, and its seq.
#[derive(Deserialize)]
pub(crate) struct Head {
    pub(crate) entry: String,
    pub(crate) seq: Option<u64>,
}

/// Reads `reader`'s next line that is not blank into `line`, replacing it, and gives the entry it holds: the one
/// walk of a log's lines that the uploader's reads of segments and the reader's reads of ranges share. `read` is
/// called with the bytes of each line read, blank ones included.
/// Error cases:
/// - the reader fails, `read` fails, or the line is not an entry
pub(crate) fn next_entry(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    mut read: impl FnMut(usize) -> anyhow::Result<()>,
) -> anyhow::Result<Option<Head>> {
    loop {
        line.clear();
        let n = reader.read_until(b'\n', line).context("reading a line")?;
        if n == 0 {
            return Ok(None);
        }
        read(n)?;
        if !line.trim_ascii().is_empty() {
            return serde_json::from_slice(line).map(Some).context("a line that is not a log entry");
        }
    }
}

/// The key prefix of a log's objects: its custody chain and vault, which no other log shares, since a vault
/// takes checkpoints of one log only.
pub(crate) fn log_prefix(chain_id: u64, vault: AccountId) -> String {
    format!("log/{chain_id}/{vault}/")
}

/// The key of range `index`'s object, under the log's `prefix`.
pub(crate) fn range_key(prefix: &str, index: u64) -> String {
    format!("{prefix}{:020}.jsonl.zst", index * RANGE_SEQS)
}

/// The key prefix of the tail of the checkpoint at `seq` rooted at `root`, under the log's `prefix`, which a reader
/// derives from the vault's `latestSeq` and `latestRoot`: each piece of it is named under it as a range is.
pub(crate) fn tail_prefix(prefix: &str, seq: u64, root: &B256) -> String {
    format!("{prefix}tail/{seq:020}-0x{}/", hex::encode(root))
}

/// How many ranges, from the first, `view`'s completed segments hold whole.
/// Error cases:
/// - the last listing cannot be read
pub(crate) fn whole_ranges(view: &Manifest) -> anyhow::Result<u64> {
    Ok(view.unlisted_from()? / RANGE_SEQS)
}

/// The bytes of range `index`: the lines of every entry whose seq is in it, in log order, read from `view`'s
/// completed segments in `dir`, each checked against its listing first.
/// Error cases:
/// - `view` is not a writer's, or no completed segment holds the range's first or last seq
/// - a listing or a segment cannot be read, a segment differs from its listing, or a line is not an entry
#[cfg(test)]
pub(crate) fn range_bytes(dir: &Path, view: &Manifest, index: u64) -> anyhow::Result<Vec<u8>> {
    range_bytes_with(dir, view, index, &BTreeMap::new())
}

/// `range_bytes`, reading each completed segment named in `supplied` from the bytes it maps it to, checked against
/// its listing by the caller, rather than from its file: a segment the host no longer holds, as read from the bucket.
/// Error cases:
/// - as `range_bytes`
pub(crate) fn range_bytes_with(
    dir: &Path,
    view: &Manifest,
    index: u64,
    supplied: &BTreeMap<String, Vec<u8>>,
) -> anyhow::Result<Vec<u8>> {
    let (first, last) = (index * RANGE_SEQS, (index + 1) * RANGE_SEQS - 1);
    ensure!(last < view.unlisted_from()?, "range {index}: seq {last} is not in a completed segment yet");
    let mut range = Vec::new();
    entries_of((dir, supplied), view, first..=last, |_, line| {
        range.extend_from_slice(line);
        Ok(())
    })
    .with_context(|| format!("range {index}"))?;
    Ok(range)
}

/// The bytes of piece `index` of the tail of the checkpoint at `seq`: the lines of range `index` that `view`'s copy
/// holds in `dir`, read from its completed segments and its writer's active prefix, every one of them for a range
/// before the checkpoint's, and for the checkpoint's own range those up to and including the checkpoint's entry.
/// Error cases:
/// - `index` is past the checkpoint's range, or the copy holds no command after a range before it
/// - the copy holds no checkpoint at `seq`
/// - as `range_bytes`, the active prefix's hash standing for a completed segment's listing
pub(crate) fn piece_bytes(dir: &Path, view: &Manifest, index: u64, seq: u64) -> anyhow::Result<Vec<u8>> {
    let last = seq / RANGE_SEQS;
    ensure!(index <= last, "range {index} is past the checkpoint at seq {seq}");
    let first = index * RANGE_SEQS;
    let end = if index < last { first + RANGE_SEQS - 1 } else { seq };
    let (mut piece, mut through) = (Vec::new(), None);
    let whole = entries_of((dir, &BTreeMap::new()), view, first..=end, |head, line| {
        piece.extend_from_slice(line);
        if head.entry == "checkpoint" && head.seq == Some(seq) {
            through = Some(piece.len());
        }
        Ok(())
    })
    .with_context(|| format!("piece {index} of the tail at seq {seq}"))?;
    if index < last {
        // Every entry of the range's last seq is in the copy once a later command is.
        ensure!(whole, "piece {index} of the tail at seq {seq}: the copy holds no command after seq {end}");
        return Ok(piece);
    }
    piece.truncate(through.with_context(|| format!("the copy holds no checkpoint at seq {seq}"))?);
    Ok(piece)
}

/// The root of the checkpoint `view`'s copy holds at `seq`, read from its completed segments in `dir`; `None` when
/// the copy holds no checkpoint there.
/// Error cases:
/// - as `range_bytes`, with no completed segment holding `seq`
pub(crate) fn checkpoint_root(dir: &Path, view: &Manifest, seq: u64) -> anyhow::Result<Option<String>> {
    ensure!(seq < view.unlisted_from()?, "seq {seq} is not in a completed segment yet");
    let mut root = None;
    entries_of((dir, &BTreeMap::new()), view, seq..=seq, |head, line| {
        if head.entry == "checkpoint"
            && let LogEntry::Checkpoint(checkpoint) = serde_json::from_slice(line).context("a checkpoint")?
            && checkpoint.seq == seq
        {
            root = Some(checkpoint.root);
        }
        Ok(())
    })?;
    Ok(root)
}

/// The listings of `view`'s completed segments that hold seqs of range `index` and whose files are not in `dir`, as
/// when the log's local window removed them once the bucket held every range they hold, or the host lost them.
/// Error cases:
/// - no completed segment holds the range's first or last seq, a listing cannot be read, or `dir` cannot be probed
pub(crate) fn absent_segments(dir: &Path, view: &Manifest, index: u64) -> anyhow::Result<Vec<SegmentInfo>> {
    let (first, last) = (index * RANGE_SEQS, (index + 1) * RANGE_SEQS - 1);
    ensure!(last < view.unlisted_from()?, "seq {last} is not in a completed segment yet");
    let (mut ordinal, _) =
        view.containing(first)?.with_context(|| format!("no completed segment holds seq {first}"))?;
    let mut absent = Vec::new();
    loop {
        let listed = view.completed_at(ordinal)?;
        let path = dir.join(&listed.file);
        if !path.try_exists().with_context(|| format!("probing {}", path.display()))? {
            absent.push(listed.clone());
        }
        if listed.last_seq >= last {
            return Ok(absent);
        }
        ordinal += 1;
    }
}

/// Calls `each` with every line of `view`'s segments in `dir` whose entry belongs to a seq in `seqs`, in log order:
/// a command's own line and the lines after it up to the next command, from the completed segments and then the
/// writer's active prefix. Each segment is checked against its listing, and the active prefix against its hash, and
/// read a line at a time. Returns whether the copy holds every entry of the last of `seqs`: a later command or the
/// end of a completed segment says so, where the end of the active prefix does not.
/// Error cases:
/// - `view` is not a writer's, or no segment holds the first of `seqs`
/// - a listing or a segment cannot be read, a segment differs from its listing or the prefix from its hash, a line
///   is not an entry, or `each` fails
fn entries_of(
    (dir, supplied): (&Path, &BTreeMap<String, Vec<u8>>),
    view: &Manifest,
    seqs: std::ops::RangeInclusive<u64>,
    mut each: impl FnMut(&Head, &[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    view.ensure_verified()?;
    let (first, last) = (*seqs.start(), *seqs.end());
    let completed = view.completed_len();
    let mut ordinal = view.containing(first)?.map_or(completed, |(ordinal, _)| ordinal);
    let mut line = Vec::new();
    loop {
        let (segment, file, ends): (Box<dyn Read>, _, _) = match ordinal < completed {
            true => {
                let listed = view.completed_at(ordinal)?;
                let read: Box<dyn Read> = match supplied.get(&listed.file) {
                    Some(bytes) => Box::new(Cursor::new(bytes.clone())),
                    None => Box::new(open_segment_archive(dir, &listed)?),
                };
                (read, listed.file, Some(listed.last_seq))
            }
            false => {
                let (active, prefix) =
                    open_active_archive(dir, view)?.with_context(|| format!("no segment holds seq {first}"))?;
                (Box::new(prefix), active.info.file, None)
            }
        };
        let mut segment = BufReader::new(segment);
        let mut seq = None;
        while let Some(head) = next_entry(&mut segment, &mut line, |_| Ok(())).with_context(|| file.clone())? {
            if head.entry == "command" {
                seq = Some(head.seq.with_context(|| format!("a command of {file} names no seq"))?);
            }
            let seq = seq.with_context(|| format!("{file} starts with no command"))?;
            if seq > last {
                return Ok(true);
            }
            if seq >= first {
                each(&head, &line)?;
            }
        }
        match ends {
            Some(ends) if ends >= last => return Ok(true),
            Some(_) => ordinal += 1,
            None => return Ok(false),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
