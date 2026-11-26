//! A damaged derived history is rebuilt through an in-memory trusted complete command prefix. A rebuild reads
//! every command, which the trusted prefix binds, and derives no engine state but the rows of the rollovers the
//! history's store does not hold, from the newest daily snapshot before each (`Rederive`).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use engine_core::Engine;
use engine_core::event::Event;
use engine_core::types::Millis;
use tracing::warn;

use crate::history::authenticated::Index;
use crate::history::derived::{Derived, Rows, RowsStore, Taken, apply_logged, engine_from, goes_on};
use crate::history::rebuild::{Fingerprinter, ReplayFingerprint};
use crate::log::{CommandEntry, LogEntry, LogReader, Offset, SegmentedLog, Stop};
use crate::sequencer::ReplayedLog;

/// Commands a rebuild indexes per update, matching the history owner's per-turn bound.
const REBUILD_BATCH: usize = 1024;

/// The derived rows a rebuild takes: from the history's store, or from a replay of an engine of its own, kept
/// while the rebuild goes on.
pub(super) struct Rederive<'a> {
    dir: &'a Path,
    store: &'a RowsStore,
    engine: Option<Engine>,
}

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

impl<'a> Rederive<'a> {
    /// Rows of the log in `dir`, from `store` or derived again into it.
    pub(super) fn new(dir: &'a Path, store: &'a RowsStore) -> Self {
        Self { dir, store, engine: None }
    }

    /// The derived rows of `command`, which the log holds and a repair takes next: none when its logged events
    /// cover none; from the store when its file gives the logged aggregates; otherwise from this replay, which
    /// goes on when its engine stands before `command` and not before the newest daily snapshot before it, and
    /// otherwise starts again there, or at genesis (`engine_from`), and are stored (`RowsStore::keep`).
    /// Error cases:
    /// - the log cannot be read from where the replay stands (`log::NotLocal` before the first segment on this
    ///   host), or ends before `command`, or holds another command at its seq
    /// - a command does not apply, or its logged events differ from the engine's (`apply_logged`)
    /// - the rows are not those `command`'s aggregates cover, or `stop` is set
    pub(super) fn rows(&mut self, command: &CommandEntry, stop: &AtomicBool) -> Result<Option<Rows>> {
        let (seq, events) = (command.sequenced.seq, command.events.as_deref().unwrap_or_default());
        if !events.iter().any(|event| matches!(event, Event::FinancingApplied { positions, .. } if *positions > 0)) {
            return Ok(None);
        }
        match self.store.derived(command) {
            Ok(Some(stored)) => return Ok(Some(stored.rows)),
            Ok(None) => {}
            Err(e) => warn!(seq, "the stored rows are not the logged ones, so they are derived again: {e:#}"),
        }
        let snapshots = self.store.snapshots();
        let mut engine = match self.engine.take() {
            Some(engine) if goes_on(engine.last_seq(), seq, snapshots) => engine,
            _ => engine_from(snapshots, |listing| listing.seq < seq),
        };
        let from = engine.last_seq().map_or(0, |last| last + 1);
        let mut reached = false;
        let read = crate::log::read_from(self.dir, from, |entry| {
            ensure!(!stop.load(Ordering::Acquire), "history repair cancelled");
            let LogEntry::Command(logged) = entry else { return Ok(()) };
            let at = logged.sequenced.seq;
            if at < from {
                return Ok(());
            }
            apply_logged(&mut engine, &logged)?;
            if at == seq {
                ensure!(logged == *command, "the log holds another command at seq {seq}");
                reached = true;
                return Err(Stop.into());
            }
            Ok(())
        });
        match read {
            Err(error) if error.is::<Stop>() => {}
            Err(error) => return Err(error),
            Ok(()) => {}
        }
        ensure!(reached, "the log ends before seq {seq}, whose rows the repair takes");
        let derived = Derived::of(seq, events, engine.take_derived())?;
        self.engine = Some(engine);
        let rows = derived.map(|derived| derived.rows).context("the rollover derived no rows")?;
        self.store.keep(seq, command.sequenced.ts, &rows);
        Ok(Some(rows))
    }
}

/// Rebuilds the index from every command the log in `dir` holds, read through `view`, the first pass's actual
/// byte commitments, never a newly loaded unsigned manifest, each with its derived rows (`Rederive`), which are
/// written to `rows`. The entries `expected` fingerprints, every one, must be the ones it replayed; the index's
/// command prefix, which the caller compares with the replay's (`ReplayedLog::verify_history_tip`), binds the
/// commands.
/// Error cases:
/// - checked rereading, a command's rows (`Rederive::rows`), exact history updates or the fingerprint comparison
///   fails
pub(crate) fn rebuild_history_view(
    dir: &Path,
    view: crate::log::Manifest,
    expected: &ReplayFingerprint,
    rows: &RowsStore,
    index: &mut Index,
) -> Result<()> {
    let mut reader = LogReader::open(dir, Offset::start_of(0), tokio::sync::watch::channel(view).1)?;
    let mut fingerprint = Fingerprinter::new();
    let (mut derive, running) = (Rederive::new(dir, rows), AtomicBool::new(false));
    let mut batch = Vec::with_capacity(REBUILD_BATCH);
    while let Some((_, entry)) = reader.next_entry()? {
        fingerprint.observe(&entry)?;
        if let LogEntry::Command(command) = entry {
            let derived = derive.rows(&command, &running)?;
            batch.push(Taken { command, derived });
            if batch.len() == REBUILD_BATCH {
                index.ingest_taken(&batch)?;
                batch.clear();
            }
        }
    }
    index.ingest_taken(&batch)?;
    expected.verify(&fingerprint.finish())
}

/// Rebuilds only into a fresh unpublished generation, never from the damaged SQL rows, from every command the log
/// in `dir` holds through the trusted head `expected` (its seq, stamp and command prefix), which binds them, each
/// with its derived rows (`Rederive`), written to `rows`. It runs beside the live sequencer, which keeps appending
/// and rotating.
/// Error cases:
/// - source IO, a command's rows (`Rederive::rows`), a verified history update or the expected head check fails
pub(crate) fn rebuild_history_prefix(
    dir: &Path,
    expected: (u64, Millis, [u8; 32]),
    rows: &RowsStore,
    index: &mut Index,
    stopped: &AtomicBool,
) -> Result<()> {
    let mut derive = Rederive::new(dir, rows);
    let mut reached = false;
    let mut batch = Vec::with_capacity(REBUILD_BATCH);
    let result = SegmentedLog::open_while_writing(dir, |entry| {
        ensure!(!stopped.load(Ordering::Acquire), "history repair cancelled");
        let LogEntry::Command(command) = entry else { return Ok(()) };
        let seq = command.sequenced.seq;
        ensure!(seq <= expected.0, "repair source skipped its required command");
        let derived = derive.rows(&command, stopped)?;
        batch.push(Taken { derived, command });
        if seq == expected.0 {
            index.ingest_taken(&batch)?;
            reached = true;
            return Err(ReachedPrefix.into());
        }
        if batch.len() == REBUILD_BATCH {
            index.ingest_taken(&batch)?;
            batch.clear();
        }
        Ok(())
    });
    match result {
        Err(error) if error.downcast_ref::<ReachedPrefix>().is_some() => {}
        Err(error) => return Err(error),
        Ok(_) => ensure!(reached, "repair log ends before the required history prefix"),
    }
    let head = index.head();
    ensure!(
        head.last_seq() == Some(expected.0) && head.last_ts() == Some(expected.1),
        "repair index progress differs from trusted history"
    );
    ensure!(head.command_prefix == expected.2, "repair index command prefix differs from trusted history");
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
