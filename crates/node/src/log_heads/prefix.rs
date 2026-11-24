//! Binding the on-chain log head to a snapshot's history without paying for full engine replay.
//! The snapshot proves which state its signer produced; it does not prove that it is the history
//! the posted head names. A posted head at or under the snapshot's base is bound by `since`: the
//! commands after the head, through the base, hashed onto the head's cumulative command-prefix
//! digest, must give the snapshot's certified `context.command_prefix`. The prefix is a hash chain,
//! so a match proves the snapshot's history holds the head, and only the commands since the head
//! are read. A mismatch refuses startup. With no head posted, `verify` hashes and seal-checks the
//! on-disk prefix from the log's start through the base, binding it to the snapshot, and stamps
//! the command a vault-finalized checkpoint under the base covers, for the stall alarm. A cache's
//! history head before its checkpoint is bound the same way, by `bind_history`.

use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};
use tracing::error;

use super::Head;
use crate::log::{self, CheckpointDomain, LogEntry, ReplayWorkers, SegmentedLog, Stop};
use crate::sequencer::origin::Origin;

/// Marks a posted-head refusal below the snapshot base: the candidate loop must not treat it as a
/// rejectable cache and fall back — the node refuses to start.
#[derive(Debug)]
pub(crate) struct Rollback;

impl std::fmt::Display for Rollback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the on-chain log head conflicts with this log below the snapshot base")
    }
}

impl std::error::Error for Rollback {}

/// What the anchor pass binds to the snapshot: the running digest at the base, and the sequenced
/// time of the command at the vault-finalized checkpoint, where the stall alarm starts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Verified {
    pub base_prefix: [u8; 32],
    pub anchor_ts: Millis,
}

/// Whether the log's commands after `posted`, through the snapshot base `base`, extend the head's
/// cumulative command-prefix digest to the snapshot's certified one. Only the segments from the one
/// holding the head's next command are read, and no seal is checked: the digest comparison and the
/// snapshot's signature are the binding. Returns the stamp the stall alarm starts from: the first
/// command after the head's, which the comparison binds, or `base_ts`, the snapshot's own, when the
/// head is at the base.
/// Error cases:
/// - the log cannot be read, skips a seq, or ends before the base: a rejected cache
/// - the digests differ: a `Rollback` (the Critical names the seq)
pub(crate) fn since(
    dir: &Path,
    posted: Head,
    base: u64,
    base_ts: Millis,
    certified_prefix: [u8; 32],
) -> anyhow::Result<Millis> {
    let (command_prefix, first_ts) =
        extend(dir, (posted.seq, posted.link.into()), base).context("binding the on-chain log head to the snapshot")?;
    if command_prefix != certified_prefix {
        error!(
            alert = "log_rollback",
            seq = posted.seq,
            posted_link = %posted.link,
            "the commands after the on-chain log head at seq {} do not give the snapshot's certified prefix at \
             its base {base}: the snapshot's history does not hold the head, or the log's commands since it are \
             damaged; refusing to start",
            posted.seq
        );
        return Err(anyhow::Error::new(Rollback));
    }
    Ok(first_ts.unwrap_or(base_ts))
}

/// Whether the log's commands after a paired cache's history head, `head` (its seq and cumulative
/// command-prefix digest), through the cache's checkpoint at `base`, extend the head's digest to the
/// cache's certified one, so the history the head certifies is the cache's own up to the head. Both
/// digests are signed: the head's certificate and the cache's.
/// Error cases:
/// - the log cannot be read, skips a seq, or ends before the base, or the digests differ: a rejected cache
pub(crate) fn bind_history(
    dir: &Path,
    head: (u64, [u8; 32]),
    base: u64,
    certified_prefix: [u8; 32],
) -> anyhow::Result<()> {
    let (command_prefix, _) = extend(dir, head, base).context("binding the cache's history head to its checkpoint")?;
    ensure!(
        command_prefix == certified_prefix,
        "the commands after the cache's history head at seq {} do not give its certified prefix at its checkpoint, \
         seq {base}",
        head.0
    );
    Ok(())
}

/// The cumulative command-prefix digest at `base`, extended from `from`'s (a seq and its digest) by the
/// log's commands after it, and the stamp of the first of them, if any. Only the segments from the one
/// holding the next command are read, and no seal is checked: the caller compares the digest with a signed
/// one.
/// Error cases:
/// - the log cannot be read, skips a seq, or ends before `base`
fn extend(
    dir: &Path,
    (seq, mut command_prefix): (u64, [u8; 32]),
    base: u64,
) -> anyhow::Result<([u8; 32], Option<Millis>)> {
    let (mut next, mut first_ts) = (seq + 1, None);
    if next <= base {
        let result = log::read_from(dir, next, |entry| {
            let LogEntry::Command(command) = &entry else { return Ok(()) };
            let seq = command.sequenced.seq;
            if first_ts.is_none() && seq < next {
                // The segment holding the next command starts at or before `from`.
                return Ok(());
            }
            ensure!(seq == next, "the log holds seq {seq} where seq {next} follows");
            command_prefix = log::extend_command_prefix(command_prefix, log::command_digest(command)?);
            first_ts.get_or_insert(command.sequenced.ts);
            next += 1;
            if seq == base { Err(Stop.into()) } else { Ok(()) }
        });
        match result {
            Err(cause) if !cause.is::<Stop>() => return Err(cause),
            _ => ensure!(next > base, "the log holds no command at the snapshot base {base}"),
        }
    }
    Ok((command_prefix, first_ts))
}

/// Whether the on-disk log through the snapshot base `base` carries the snapshot's certified
/// command prefix, verified from the log's start along `signer`'s seal chain, and the stamp of the
/// command at `anchor`, the vault-finalized checkpoint's seq. Only the command-prefix hash and the
/// seals are checked, ahead of the pass by `workers` — the engine is not applied — and the pass stops
/// at the base: the boundary is checked before an entry beyond it is processed, so a nonempty suffix
/// never contaminates the comparison.
/// Error cases:
/// - the log cannot be opened or a seal is invalid, an unsealed span lacks the trusted authority,
///   or the log holds no command at the anchor or the base
/// - the command prefix differs from the certified one: a rejected cache
pub(crate) fn verify(
    dir: &Path,
    domain: CheckpointDomain,
    signer: AccountId,
    anchor: u64,
    base: u64,
    certified_prefix: [u8; 32],
    workers: ReplayWorkers,
) -> anyhow::Result<Verified> {
    let mut origin = Origin::new(Some(domain), Some(signer));
    origin.check_ahead(dir, 0, workers);
    let mut command_prefix = [0u8; 32];
    let mut previous = None;
    let (mut anchor_ts, mut base_prefix) = (None, None);
    let result = SegmentedLog::open(dir, |entry| {
        let offset = log::offset_of(previous, &entry)?;
        if offset.seq > base {
            // The boundary is checked before the entry is processed: no origin update, no prefix
            // extension for the first command beyond the base.
            return Err(Stop.into());
        }
        previous = Some(offset);
        match &entry {
            LogEntry::Command(command) => {
                // The version gates only legacy-staking bookkeeping, which this pass never reads.
                origin.command(command, ProtocolVersion::LATEST)?;
                command_prefix = log::extend_command_prefix(command_prefix, log::command_digest(command)?);
                let (seq, ts) = (command.sequenced.seq, command.sequenced.ts);
                if anchor == seq {
                    anchor_ts = Some(ts);
                }
                if seq == base {
                    base_prefix = Some(command_prefix);
                }
            }
            LogEntry::Checkpoint(checkpoint) => {
                origin.checkpoint(checkpoint, domain, true)?;
            }
            LogEntry::Withdrawal(_) => {}
        }
        Ok(())
    });
    match result {
        Ok(_) => {}
        Err(cause) if cause.is::<Stop>() => {}
        Err(cause) => {
            return Err(cause).context("binding the log's prefix to the snapshot");
        }
    }
    // Unsealed spans the trusted authority does not cover refuse the start, as writable
    // recovery's coverage check does.
    origin.require_audit(signer).context("binding the log's prefix to the snapshot")?;
    // Without a posted head, a mismatch is a cache that is not this log's history, which recovery
    // rejects and replays past; only a head on chain makes it a rollback.
    ensure!(
        command_prefix == certified_prefix,
        "the log's command prefix through the snapshot base differs from the snapshot's certified prefix"
    );
    // Checked after the prefix comparison, so a log cut under the anchor is the mismatch it is.
    let anchor_ts =
        anchor_ts.with_context(|| format!("the log holds no command at the finalized checkpoint's seq {anchor}"))?;
    let base_prefix = base_prefix.context("the log holds no command at the snapshot base")?;
    Ok(Verified { base_prefix, anchor_ts })
}

#[cfg(test)]
mod tests;
