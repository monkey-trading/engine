//! A damaged derived history is rebuilt through an in-memory trusted complete command prefix.

use anyhow::{Result, ensure};
use engine_core::types::{AccountId, Millis};

use crate::history::authenticated::Index;
use crate::log::{CheckpointDomain, LogEntry, SegmentedLog};
use crate::sequencer::ReplayedLog;
use crate::sequencer::replay::Replay;
use std::sync::atomic::{AtomicBool, Ordering};

/// Commands a rebuild indexes per update, matching the history owner's per-turn bound.
const REBUILD_BATCH: usize = 1024;

impl ReplayedLog {
    /// Supplies only commitments captured by successful strict replay or authenticated suffix open.
    pub(crate) fn manifest_view(&self) -> crate::log::Manifest {
        self.log.manifest_view()
    }

    /// Binds fresh catalog derivation after all external preflight checks have authorized writes.
    /// Error cases:
    /// - authenticated catalog construction or byte-consistent descriptor rereading fails
    pub(crate) fn attach_catalog(mut self, owner: crate::log::catalog_owner::CatalogOwner) -> Result<Self> {
        self.log = self.log.attach_catalog(owner)?;
        Ok(self)
    }

    /// Resumes the separately verified catalog owner after the replay from the checkpoint to the
    /// catalog tip and the on-chain checkpoint preflight.
    /// Error cases:
    /// - current root, domain or trusted live-prefix alignment fails
    pub(crate) fn bind_catalog(mut self, owner: crate::log::catalog_owner::CatalogOwner) -> Result<Self> {
        self.log = self.log.bind_catalog_owner(owner)?;
        Ok(self)
    }

    /// Requires a migrated index to cover the actual startup command/event prefix.
    /// Error cases:
    /// - final complete command, time or prefix differs
    pub(crate) fn verify_history_tip(&self, head: &crate::history::authenticated::Head) -> Result<()> {
        ensure!(head.last_seq() == self.replayed.engine.last_seq(), "rebuilt history missed the startup tip");
        if head.last_seq().is_some() {
            ensure!(head.last_ts() == Some(self.replayed.engine.now()), "rebuilt history startup time differs");
        }
        ensure!(head.command_prefix == self.replayed.command_prefix, "rebuilt history startup prefix differs");
        Ok(())
    }
}

/// Uses the first pass's actual byte commitments, never a newly loaded unsigned manifest.
/// Error cases:
/// - checked rereading, strict replay, exact history updates or full-prefix comparison fails
pub(crate) fn rebuild_history_view(
    dir: &std::path::Path,
    view: crate::log::Manifest,
    expected: &crate::history::rebuild::ReplayFingerprint,
    domain: CheckpointDomain,
    signer: Option<AccountId>,
    index: &mut Index,
) -> Result<()> {
    let mut reader =
        crate::log::LogReader::open(dir, crate::log::Offset::start_of(0), tokio::sync::watch::channel(view).1)?;
    let mut replay = Replay::with_trust(Some(domain), signer);
    let mut fingerprint = crate::history::rebuild::Fingerprinter::new();
    let mut batch = Vec::with_capacity(REBUILD_BATCH);
    while let Some((_, entry)) = reader.next_entry()? {
        replay.apply(&entry)?;
        fingerprint.observe(&entry)?;
        if let LogEntry::Command(command) = entry {
            batch.push(command);
            if batch.len() == REBUILD_BATCH {
                index.ingest_batch(&batch)?;
                batch.clear();
            }
        }
    }
    index.ingest_batch(&batch)?;
    expected.verify(&fingerprint.finish())
}

/// Rebuilds only into a fresh unpublished generation, never from the damaged SQL rows.
/// Error cases:
/// - strict replay, source IO, a verified history update or the exact expected prefix check fails
pub(crate) fn rebuild_history_prefix(
    dir: &std::path::Path,
    domain: CheckpointDomain,
    signer: Option<AccountId>,
    expected: (u64, Millis, [u8; 32]),
    index: &mut Index,
    stopped: &AtomicBool,
) -> Result<()> {
    let mut replay = Replay::with_trust(Some(domain), signer);
    let mut reached = false;
    let mut batch = Vec::with_capacity(REBUILD_BATCH);
    // The sequencer keeps appending and rotating while history repairs.
    let result = SegmentedLog::open_while_writing(dir, |entry| {
        ensure!(!stopped.load(Ordering::Acquire), "history repair cancelled");
        replay.apply(&entry)?;
        if let LogEntry::Command(command) = entry {
            let seq = command.sequenced.seq;
            ensure!(seq <= expected.0, "repair source skipped its required command");
            batch.push(command);
            if seq == expected.0 {
                index.ingest_batch(&batch)?;
                reached = true;
                return Err(ReachedPrefix.into());
            }
            if batch.len() == REBUILD_BATCH {
                index.ingest_batch(&batch)?;
                batch.clear();
            }
        }
        Ok(())
    });
    match result {
        Err(error) if error.downcast_ref::<ReachedPrefix>().is_some() => {}
        Err(error) => return Err(error),
        Ok(_) => ensure!(reached, "repair log ends before the required history prefix"),
    }
    let replay = replay.finish();
    ensure!(replay.engine.last_seq() == Some(expected.0), "repair replay cursor differs from trusted history");
    ensure!(replay.engine.now() == expected.1, "repair replay time differs from trusted history");
    ensure!(replay.command_prefix == expected.2, "repair replay command prefix differs from trusted history");
    let head = index.head();
    ensure!(
        head.last_seq() == Some(expected.0) && head.last_ts() == Some(expected.1),
        "repair index progress differs from replay"
    );
    ensure!(head.command_prefix == expected.2, "repair index command prefix differs from replay");
    Ok(())
}

/// Stops the full-audit reader after the trusted command without exposing a partial log writer.
#[derive(Debug)]
struct ReachedPrefix;

impl std::fmt::Display for ReachedPrefix {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("reached trusted history repair prefix")
    }
}

impl std::error::Error for ReachedPrefix {}
