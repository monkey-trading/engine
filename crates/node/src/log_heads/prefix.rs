//! Binding the on-chain log head to a snapshot's history without paying for full engine replay.
//! The snapshot proves which state its signer produced; it does not prove that it is the history
//! the posted head names. A posted head at or under the snapshot's base is bound by `since`: the
//! commands after the head, through the base, hashed onto the head's cumulative command-prefix
//! digest, must give the snapshot's certified `context.command_prefix`. The prefix is a hash chain,
//! so a match proves the snapshot's history holds the head, and only the commands since the head
//! are read. A mismatch refuses startup. With no head posted, `verify` hashes and seal-checks the
//! on-disk prefix from the log's start, or from a signed daily snapshot's checkpoint with the command
//! prefix it carries (`verify_from`), through the base, binding it to the snapshot, and stamps the
//! command a vault-finalized checkpoint under the base covers, for the stall alarm. A cache's history
//! head before its checkpoint is bound the same way, by `bind_history`. A start from a daily snapshot
//! binds a posted head at or under its checkpoint to the history the snapshot carries
//! (`under_snapshot`).

use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};
use tracing::error;

use super::Head;
use crate::daily_snapshots::Signed;
use crate::log::{self, Checkpoint, CheckpointDomain, LogEntry, Offset, ReplayWorkers, SegmentedLog, Stop};
use crate::sequencer::origin::Origin;
use crate::sequencer::replay::Carried;

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

/// Where the anchor pass starts in place of the log's start: a signed daily snapshot's checkpoint, at `offset`,
/// with the cumulative command prefix and the stamp the snapshot carries for the command it covers.
#[derive(Clone, Copy)]
pub(crate) struct SnapshotStart<'a> {
    pub offset: Offset,
    pub checkpoint: &'a Checkpoint,
    pub command_prefix: [u8; 32],
    pub ts: Millis,
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
        return Err(rollback(posted, base));
    }
    Ok(first_ts.unwrap_or(base_ts))
}

/// For a start from the daily snapshot `start`, binds `posted`, a head at or under its checkpoint, to the history
/// the snapshot carries (`under_snapshot`), and returns the stamp the stall alarm starts from; none for no head, or
/// one past the checkpoint, which the replay of the log after it binds.
/// Error cases:
/// - the snapshot carries no replay state, or as `under_snapshot`
pub(crate) fn under_daily(dir: &Path, posted: Option<Head>, start: &Signed) -> anyhow::Result<Option<Millis>> {
    let snapshot = &start.snapshot;
    let Some(posted) = posted.filter(|posted| posted.seq <= snapshot.offset.seq) else { return Ok(None) };
    let carried = snapshot.carried.as_ref().context("the daily snapshot carries no replay state")?;
    under_snapshot(dir, posted, carried, (snapshot.offset.seq, snapshot.engine.now())).map(Some)
}

/// Whether a head `posted` at or under a daily snapshot's checkpoint, at `base` and stamped `base_ts`, is in the
/// history the snapshot carries: the link the carried links hold at its seq, when they hold it, or else the
/// commands after it through the base hashed onto its link (`since`), must give the snapshot's. Returns the stamp
/// the stall alarm starts from, as `since` does.
/// Error cases:
/// - as `since`: a link or digest of another history is a `Rollback`
fn under_snapshot(
    dir: &Path,
    posted: Head,
    carried: &Carried,
    (base, base_ts): (u64, Millis),
) -> anyhow::Result<Millis> {
    let Some(at) = carried.links.iter().position(|link| link.seq == posted.seq) else {
        return since(dir, posted, base, base_ts, carried.command_prefix.into());
    };
    if carried.links[at].link != posted.link {
        return Err(rollback(posted, base));
    }
    Ok(carried.links.get(at + 1).map_or(base_ts, |next| next.ts))
}

/// The refusal of a head `posted` that the history through the snapshot base `base` does not hold, with its
/// Critical.
fn rollback(posted: Head, base: u64) -> anyhow::Error {
    error!(
            alert = "log_rollback",
            seq = posted.seq,
            posted_link = %posted.link,
            "the commands after the on-chain log head at seq {} do not give the snapshot's certified prefix at \
             its base {base}: the snapshot's history does not hold the head, or the log's commands since it are \
             damaged; refusing to start",
        posted.seq
    );
    anyhow::Error::new(Rollback)
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
#[cfg(test)]
pub(crate) fn verify(
    dir: &Path,
    domain: CheckpointDomain,
    signer: AccountId,
    anchor: u64,
    base: u64,
    certified_prefix: [u8; 32],
    workers: ReplayWorkers,
) -> anyhow::Result<Verified> {
    verify_from(dir, (domain, signer), (anchor, base), certified_prefix, workers, None)
}

/// `verify`, from `start`, a signed daily snapshot's checkpoint at or under the anchor, when given: only the
/// segments from the one holding its entry are read, which must be its checkpoint, the seals are chained from
/// its link and the command prefix from the one it carries.
/// Error cases:
/// - as `verify`, or the log holds another entry at the snapshot's offset (`log::NotLocal` before the first
///   segment on this host)
/// - the snapshot's checkpoint is past the anchor
pub(crate) fn verify_from(
    dir: &Path,
    (domain, signer): (CheckpointDomain, AccountId),
    (anchor, base): (u64, u64),
    certified_prefix: [u8; 32],
    workers: ReplayWorkers,
    start: Option<SnapshotStart<'_>>,
) -> anyhow::Result<Verified> {
    let mut origin = Origin::new(Some(domain), Some(signer));
    let (mut command_prefix, mut previous, mut anchor_ts) = ([0u8; 32], None, None);
    if let Some(start) = start {
        ensure!(start.offset.seq <= anchor, "the daily snapshot's checkpoint is past the finalized checkpoint's");
        origin.link = crate::log::command_seal::checkpoint_link(start.checkpoint)?;
        (command_prefix, anchor_ts) = (start.command_prefix, (start.offset.seq == anchor).then_some(start.ts));
    }
    origin.check_ahead(dir, start.map_or(0, |start| start.offset.seq + 1), workers);
    let mut base_prefix = None;
    let mut visit = |entry: LogEntry| {
        if let Some(start) = start.filter(|_| previous.is_none()) {
            let held = matches!(&entry, LogEntry::Checkpoint(held) if held == start.checkpoint);
            ensure!(held, "the log holds another entry at {} than the daily snapshot's checkpoint", start.offset);
            previous = Some(start.offset);
            return Ok(());
        }
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
    };
    let result = match start {
        Some(start) => SegmentedLog::open_from(dir, start.offset, &mut visit),
        None => SegmentedLog::open(dir, &mut visit),
    };
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
