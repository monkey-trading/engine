//! The financing rows of the rollovers before a replica's daily snapshot. A replica built from a snapshot applies
//! only the commands after it, so it writes no rows of the rollovers before it to the history's store, which a
//! whole replay writes as it applies them, and a member's history index, which takes every command from the log's
//! start at its first turn, would replay the log from its start for them (`history_rows_replayed`). So, once the
//! replica is built, a thread of its own derives them into the store while the member follows
//! (`RowsStore::derive_through`). The replica's drop stops it without waiting for it, so a turn that comes first
//! starts at once and leaves the rest to the history index, as before; the thread holds nothing the node reads, and
//! ends at its next command.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{info, warn};

use crate::history::derived::RowsStore;

/// The thread deriving the rows before a snapshot, told to stop when dropped.
pub(super) struct RowsBefore {
    stop: Arc<AtomicBool>,
}

impl Drop for RowsBefore {
    /// Stops the thread at its next command, not waiting for it.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl RowsBefore {
    /// Starts deriving the rows of the copy in `dir` through the command at `through`, the snapshot's, into
    /// `store`. A thread that cannot start leaves them to the history index, with a Warning.
    pub(super) fn start(dir: &Path, through: u64, store: Arc<RowsStore>) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (dir, stopping) = (dir.to_owned(), stop.clone());
        let thread = std::thread::Builder::new().name("rows-before-snapshot".into()).spawn(move || {
            match store.derive_through(&dir, through, &stopping) {
                Ok(Some(kept)) => info!(through, kept, "derived the financing rows before the replica's snapshot"),
                Ok(None) => {}
                Err(e) => warn!(
                    alert = "financing_rows_unstored",
                    through, "the financing rows before the replica's snapshot are left to the history index: {e:#}"
                ),
            }
        });
        match thread {
            Ok(_) => Some(Self { stop }),
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
