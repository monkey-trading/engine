//! The archive's ranges: `RANGE_SEQS` sequence numbers each, built from whichever of a copy's completed segments
//! hold them. A command's entries never straddle two segments and every segment starts with a command, so a range
//! is whole once a completed segment holds its last seq, and its bytes are the same however the copy's segments
//! are cut.

use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::types::AccountId;
use serde::Deserialize;

use crate::log::{LogEntry, Manifest, open_segment_archive};

/// How many sequence numbers one object of the archive holds: 10,000, about 5.5 MB of launch flow and under a
/// megabyte compressed; six in tests, so ranges cross the test log's four-command segments.
pub(crate) const RANGE_SEQS: u64 = if cfg!(test) { 6 } else { 10_000 };

/// What a log line's entry is, and a command's seq.
#[derive(Deserialize)]
struct Head {
    entry: String,
    seq: Option<u64>,
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
pub(crate) fn range_bytes(dir: &Path, view: &Manifest, index: u64) -> anyhow::Result<Vec<u8>> {
    let (first, last) = (index * RANGE_SEQS, (index + 1) * RANGE_SEQS - 1);
    let mut range = Vec::new();
    entries_of(dir, view, first..=last, |_, line| {
        range.extend_from_slice(line);
        Ok(())
    })
    .with_context(|| format!("range {index}"))?;
    Ok(range)
}

/// The root of the checkpoint `view`'s copy holds at `seq`, read from its completed segments in `dir`; `None` when
/// the copy holds no checkpoint there.
/// Error cases:
/// - as `range_bytes`, with no completed segment holding `seq`
pub(crate) fn checkpoint_root(dir: &Path, view: &Manifest, seq: u64) -> anyhow::Result<Option<String>> {
    let mut root = None;
    entries_of(dir, view, seq..=seq, |head, line| {
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

/// Calls `each` with every line of `view`'s completed segments in `dir` whose entry belongs to a seq in `seqs`,
/// in log order: a command's own line and the lines after it up to the next command. Each segment is checked
/// against its listing, and read a line at a time.
/// Error cases:
/// - `view` is not a writer's, or no completed segment holds the first or last of `seqs`
/// - a listing or a segment cannot be read, a segment differs from its listing, a line is not an entry, or
///   `each` fails
fn entries_of(
    dir: &Path,
    view: &Manifest,
    seqs: std::ops::RangeInclusive<u64>,
    mut each: impl FnMut(&Head, &[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    view.ensure_verified()?;
    let (first, last) = (*seqs.start(), *seqs.end());
    ensure!(last < view.unlisted_from()?, "seq {last} is not in a completed segment yet");
    let (mut ordinal, _) =
        view.containing(first)?.with_context(|| format!("no completed segment holds seq {first}"))?;
    let mut line = Vec::new();
    loop {
        let listed = view.completed_at(ordinal)?;
        let mut segment = BufReader::new(open_segment_archive(dir, &listed)?);
        let mut seq = None;
        loop {
            line.clear();
            if segment.read_until(b'\n', &mut line).with_context(|| format!("reading {}", listed.file))? == 0 {
                break;
            }
            if line.trim_ascii().is_empty() {
                continue;
            }
            let head: Head = serde_json::from_slice(&line).with_context(|| format!("a line of {}", listed.file))?;
            if head.entry == "command" {
                seq = Some(head.seq.with_context(|| format!("a command of {} names no seq", listed.file))?);
            }
            let seq = seq.with_context(|| format!("{} starts with no command", listed.file))?;
            if seq > last {
                return Ok(());
            }
            if seq >= first {
                each(&head, &line)?;
            }
        }
        if listed.last_seq >= last {
            return Ok(());
        }
        ordinal += 1;
    }
}

#[cfg(test)]
pub(crate) mod tests;
