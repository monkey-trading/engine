//! The financing rows of the rollovers before a replica's daily snapshot. A replica built from a snapshot applies
//! only the commands after it, so it writes no rows of the rollovers before it to the history's store, which a
//! whole replay writes as it applies them, and a member's history index, which takes every command from the log's
//! start at its first turn, would replay the log from its start for them (`history_rows_replayed`). So, once the
//! replica is built, a thread of its own applies the copy from its start through the snapshot's command to an
//! engine of its own, which it checks against the commands' logged events, and keeps each rollover's rows in the
//! store, while the member follows. The replica's drop stops it: a turn that comes first leaves the rest to the
//! history index, as before. A copy that starts past genesis, as one taken over from a state no log holds does,
//! has no rows to derive.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use anyhow::{Context, ensure};
use engine_core::Engine;
use tracing::{info, warn};

use crate::history::derived::{Derived, RowsStore};
use crate::log::{LogEntry, SegmentedLog, Stop};

/// The thread deriving the rows before a snapshot; stopped and joined when dropped.
pub(super) struct RowsBefore {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for RowsBefore {
    /// Stops the thread at its next command and joins it.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // A thread that panicked kept no more rows; the history index derives them.
            let _ = thread.join();
        }
    }
}

impl RowsBefore {
    /// Starts deriving the rows of the copy in `dir` through the command at `through`, the snapshot's, into
    /// `store`. A thread that cannot start leaves them to the history index, with a Warning.
    pub(super) fn start(dir: &Path, through: u64, store: Arc<RowsStore>) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (dir, stopping) = (dir.to_owned(), stop.clone());
        let thread = std::thread::Builder::new().name("rows-before-snapshot".into()).spawn(move || {
            match derive(&dir, through, &store, &stopping) {
                Ok(Some(kept)) => info!(through, kept, "derived the financing rows before the replica's snapshot"),
                Ok(None) => {}
                Err(e) => warn!(
                    alert = "financing_rows_unstored",
                    through, "the financing rows before the replica's snapshot are left to the history index: {e:#}"
                ),
            }
        });
        match thread {
            Ok(thread) => Some(Self { stop, thread: Some(thread) }),
            Err(e) => {
                warn!(
                    alert = "financing_rows_unstored",
                    through, "the financing rows before the replica's snapshot are left to the history index: {e}"
                );
                None
            }
        }
    }
}

/// Applies the copy in `dir` from its start through the command at `through` to an engine of its own and keeps
/// each command's rows in `store`, until `stop` is set; how many commands' rows it kept, or none for a copy that
/// starts past genesis or a derivation stopped.
/// Error cases:
/// - the copy cannot be read, a command does not apply, or its logged events differ from the engine's
/// - a command's rows are not those its events' aggregates cover
fn derive(dir: &Path, through: u64, store: &RowsStore, stop: &AtomicBool) -> anyhow::Result<Option<u64>> {
    let (mut engine, mut kept, mut ended) = (Engine::new(), 0u64, None);
    SegmentedLog::read_span_ahead(dir, 0, |entry| {
        if stop.load(Ordering::Acquire) {
            return Err(Stop.into());
        }
        let LogEntry::Command(command) = entry else { return Ok(()) };
        let seq = command.sequenced.seq;
        if engine.last_seq().is_none() && seq != 0 {
            return Err(Stop.into());
        }
        let events = engine.apply(&command.sequenced).with_context(|| format!("applying seq {seq}"))?;
        // No root is computed, the only thing that drains the engine's record of changed keys.
        engine.discard_changed_keys();
        ensure!(command.events.as_ref() == Some(&events), "events logged at seq {seq} differ from the engine's");
        if let Some(derived) = Derived::of(seq, &events, engine.take_derived())? {
            store.keep(seq, command.sequenced.ts, &derived.rows);
            kept += 1;
        }
        if seq >= through {
            ended = Some(kept);
            return Err(Stop.into());
        }
        Ok(())
    })?;
    Ok(ended)
}
