//! Reading the log from the archive's bucket alone, through a checkpoint the vault holds: the bucket cannot say where
//! the log ends, and the vault's `latestSeq` and `latestRoot` name the checkpoint and its tail (`tail_prefix`). The
//! reader takes the log's ranges from the first until the first the bucket lacks, then the tail's pieces, shaped as
//! ranges, from there to the checkpoint's own, and keeps the lines up to and including the checkpoint's entry, which
//! must hold the vault's root. Nothing it reads is trusted: each object is checked as a range is (`Ranges::seqs`), and
//! the log it writes replays, its commands rebuilding the checkpoint's root, before anything relies on it.

use std::io::Write;

use anyhow::{Context, anyhow, bail, ensure};
use engine_core::eip712::B256;
use reqwest::Client;

use crate::archive::Head;
use crate::archive::RANGE_SEQS;
use crate::archive::read::{Bucket, MAX_RANGE_BYTES, Ranges, Unread};
use crate::log::LogEntry;

/// Writes to `out` the log in the bucket `log` names, through the vault's checkpoint at `seq` rooted at `root`, read
/// over `http`: the bucket's ranges until the first it lacks, then the pieces of that checkpoint's tail, the last
/// cut after the checkpoint's entry.
/// Error cases:
/// - neither the bucket's range nor the tail's piece holding a seq is in the bucket, or the bucket cannot be asked
/// - an object holds other than its range's seqs, in order from the first, or is longer than a range's bound
/// - the log read holds no checkpoint at `seq`, or holds one at another root
/// - `out` cannot be written
pub(crate) async fn read_through(
    http: &Client,
    log: &Bucket,
    (seq, root): (u64, B256),
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let last = seq / RANGE_SEQS;
    let (mut ranges, mut pieces) = (Ranges::new(log.clone()), Ranges::new(log.tail(seq, &root)?));
    let mut in_tail = false;
    for index in 0..=last {
        let wanted = (index * RANGE_SEQS, if index < last { (index + 1) * RANGE_SEQS - 1 } else { seq });
        let mut lines = None;
        if !in_tail {
            match ranges.seqs(http, wanted, MAX_RANGE_BYTES).await {
                Ok(read) => lines = Some(read),
                // The bucket's ranges are a prefix of the log, and the tail's pieces go on from the first it lacks.
                Err(Unread::Missing { .. }) => in_tail = true,
                Err(unread) => return Err(unread.into_error()),
            }
        }
        let lines = match lines {
            Some(lines) => lines,
            None => pieces.seqs(http, wanted, MAX_RANGE_BYTES).await.map_err(|unread| match unread {
                Unread::Missing { object, .. } => anyhow!(
                    "the archive's bucket holds neither range {index} at {} nor the tail's piece of it at {object}, so \
                     it does not hold the log through the checkpoint at seq {seq} rooted at {root}",
                    ranges.bucket().object_name(index)
                ),
                unread => unread.into_error(),
            })?,
        };
        let lines = if index < last { lines } else { through_checkpoint(lines, seq, &root)? };
        out.write_all(&lines).context("writing the log read from the archive's bucket")?;
    }
    Ok(())
}

/// `lines`, the last range's read through `seq`, up to and including the entry of the checkpoint at `seq`, which must
/// be rooted at `root`.
/// Error cases:
/// - a line is not an entry, or none is the checkpoint at `seq`, or that checkpoint is rooted elsewhere
fn through_checkpoint(mut lines: Vec<u8>, seq: u64, root: &B256) -> anyhow::Result<Vec<u8>> {
    let mut end = 0;
    for line in lines.split_inclusive(|byte| *byte == b'\n') {
        end += line.len();
        if line.trim_ascii().is_empty() {
            continue;
        }
        let head: Head = serde_json::from_slice(line).context("a line that is not a log entry")?;
        if head.entry != "checkpoint" || head.seq != Some(seq) {
            continue;
        }
        let LogEntry::Checkpoint(checkpoint) = serde_json::from_slice(line).context("a checkpoint")? else {
            bail!("an entry named a checkpoint is not one");
        };
        ensure!(
            checkpoint.root.parse::<B256>().is_ok_and(|held| held == *root),
            "the log in the archive's bucket holds the checkpoint at seq {seq} at root {}, not at {root}",
            checkpoint.root
        );
        lines.truncate(end);
        return Ok(lines);
    }
    bail!("the log in the archive's bucket holds no checkpoint at seq {seq}")
}

#[cfg(test)]
mod scenario;
#[cfg(test)]
pub(crate) mod tests;
