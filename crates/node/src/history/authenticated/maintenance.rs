//! Authenticated storage maintenance changes only the history root, never its command prefix.

use anyhow::{Result, ensure};

use crate::authenticated::GcReport;
use crate::history::authenticated::Index;
use crate::history::authenticated::state::checked_state;

impl Index {
    /// Adopts Store's durable maintenance root with identical command/list/queue progress.
    /// Error cases:
    /// - writer requires rebuild, failed proof/GC/durability, or changed fixed history metadata
    pub fn maintenance(&mut self, budget: usize) -> Result<GcReport> {
        ensure!(!self.requires_rebuild, "history writer requires a fresh verified generation");
        let result = (|| {
            checked_state(&self.store, &self.head)?;
            let (root, report) = self.store.gc(&self.head.root, budget)?;
            let pin = self.store.lease(&root)?;
            let mut next = self.head.clone();
            next.root = root;
            checked_state(&self.store, &next)?;
            self.head = next;
            self._visible_pin = pin;
            Ok(report)
        })();
        if result.is_err() {
            self.requires_rebuild = true;
        }
        result
    }
}
