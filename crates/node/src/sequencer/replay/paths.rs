//! The public replay entry points: whole logs and signed audit paths.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, ensure};

use engine_core::types::AccountId;

use crate::log::{self, CheckpointDomain, LogEntry, SegmentedLog};
use crate::sequencer::replay::{Replay, Replayed};

/// Replays with an independently trusted signer for withdrawal completion evidence.
/// Error cases:
/// - any entry, signature, or authorization fails replay
pub fn replay_with_signer(
    entries: &[LogEntry],
    domain: CheckpointDomain,
    signer: AccountId,
) -> anyhow::Result<Replayed> {
    let mut replay = Replay::with_trust(Some(domain), Some(signer));
    entries.iter().try_for_each(|entry| replay.apply(entry))?;
    let replayed = replay.finish();
    replayed.origin.require_audit(signer)?;
    ensure!(
        replayed.checkpoint_signers.iter().all(|named| *named == signer),
        "checkpoint signer differs from trusted signer"
    );
    Ok(replayed)
}

/// Replays the log at `path`, a single log file or a directory of segments with their manifest, as
/// it reads it, for `engine replay`. A log that holds no checkpoint verifies nothing, so it fails.
/// Error cases:
/// - nothing is at `path`, or it cannot be read
/// - the log fails its manifest check (see `SegmentedLog::open`) or replay (see `Replay::apply`)
/// - the log holds no command, or no checkpoint
pub fn replay_path(path: &Path) -> anyhow::Result<Replayed> {
    replay_path_in_domain(path, None)
}

/// Replays signatures in an explicit deployment, including logs that predate domain recording.
/// Error cases:
/// - the log fails replay or conflicts with `domain` (see `replay_path`)
pub fn replay_path_in_domain(path: &Path, domain: Option<CheckpointDomain>) -> anyhow::Result<Replayed> {
    replay_path_with_trust(path, domain, None)
}

/// Replays a published log with explicit authority for authenticating withdrawal releases.
/// Error cases:
/// - log, root, signature, domain or trusted signer validation fails
pub fn replay_path_with_trust(
    path: &Path,
    domain: Option<CheckpointDomain>,
    signer: Option<AccountId>,
) -> anyhow::Result<Replayed> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let mut replay = Replay::with_trust(domain, signer);
    if found.is_dir() {
        SegmentedLog::open(path, |entry| replay.apply(&entry))?;
    } else {
        log::read_file(path, |entry| replay.apply(&entry))?;
    }
    let replayed = replay.finish();
    if let Some(signer) = signer {
        replayed.origin.require_audit(signer)?;
        ensure!(
            replayed.checkpoint_signers.iter().all(|named| *named == signer),
            "checkpoint signer differs from trusted signer"
        );
    }
    ensure!(replayed.commands > 0, "{} holds no command", path.display());
    ensure!(replayed.checkpoints > 0, "{} holds no checkpoint, so nothing in it is verified", path.display());
    Ok(replayed)
}

/// Replays a published log whose checkpoint signer changes at signer epochs the vault records,
/// given an independently verified attestation for each signer: every checkpoint and every typed
/// withdrawal must be one of `attested`, unlike `replay_path_with_trust`'s single pinned signer.
/// Error cases:
/// - log, root, signature, or domain validation fails, or a checkpoint or typed withdrawal names
///   no attested signer
pub fn replay_path_with_attested(
    path: &Path,
    domain: Option<CheckpointDomain>,
    attested: BTreeSet<AccountId>,
) -> anyhow::Result<Replayed> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let attested_set = attested.clone();
    let mut replay = Replay::with_attested(domain, attested);
    if found.is_dir() {
        SegmentedLog::open(path, |entry| replay.apply(&entry))?;
    } else {
        log::read_file(path, |entry| replay.apply(&entry))?;
    }
    let replayed = replay.finish();
    ensure!(replayed.commands > 0, "{} holds no command", path.display());
    ensure!(replayed.checkpoints > 0, "{} holds no checkpoint, so nothing in it is verified", path.display());
    // No attested key authenticated a command tail after the last checkpoint: an unsealed
    // append must not pass as verified.
    replayed.origin.require_attested(&attested_set)?;
    Ok(replayed)
}
