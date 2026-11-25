//! The archive's ranges: `RANGE_SEQS` sequence numbers each, built from whichever of a copy's completed segments
//! hold them. A command's entries never straddle two segments and every segment starts with a command, so a range
//! is whole once a completed segment holds its last seq, and its bytes are the same however the copy's segments
//! are cut.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::types::AccountId;
use serde::Deserialize;

use crate::log::{Manifest, open_segment_archive};

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
    view.ensure_verified()?;
    let (first, last) = (index * RANGE_SEQS, (index + 1) * RANGE_SEQS - 1);
    ensure!(last < view.unlisted_from()?, "range {index} is not whole yet");
    let (mut ordinal, _) =
        view.containing(first)?.with_context(|| format!("no completed segment holds seq {first}"))?;
    let mut range = Vec::new();
    loop {
        let listed = view.completed_at(ordinal)?;
        let mut raw = Vec::with_capacity(usize::try_from(listed.bytes).context("segment length")?);
        open_segment_archive(dir, &listed)?
            .read_to_end(&mut raw)
            .with_context(|| format!("reading {}", listed.file))?;
        ensure!(raw.len() as u64 == listed.bytes, "{} is not as long as its listing", listed.file);
        let mut seq = None;
        for line in raw.split_inclusive(|byte| *byte == b'\n').filter(|line| !line.trim_ascii().is_empty()) {
            let head: Head = serde_json::from_slice(line).with_context(|| format!("a line of {}", listed.file))?;
            if head.entry == "command" {
                seq = Some(head.seq.with_context(|| format!("a command of {} names no seq", listed.file))?);
            }
            let seq = seq.with_context(|| format!("{} starts with no command", listed.file))?;
            if (first..=last).contains(&seq) {
                range.extend_from_slice(line);
            }
        }
        if listed.last_seq >= last {
            return Ok(range);
        }
        ordinal += 1;
    }
}

#[cfg(test)]
pub(crate) mod tests;
