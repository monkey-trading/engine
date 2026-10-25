//! Completing a checkpoint capture into the paired cache.

use tracing::warn;

use crate::history::runtime::Actor;
use crate::snapshot::sealing::{Capture, HistorySeal};

impl Actor {
    /// Publishes the paired cache for a capture whose checkpoint is the last entry taken, `taken`
    /// when that entry is a checkpoint; any other entry drops the capture.
    pub(super) fn complete_capture(&mut self, capture: Box<Capture>, taken: Option<&crate::log::Checkpoint>) {
        let offset = capture.snapshot.offset;
        if self.last_entry != Some(offset) || taken != Some(&capture.snapshot.checkpoint) {
            warn!(alert = "snapshot_write_failed", "history did not observe the exact captured checkpoint");
            return;
        }
        // A rename followed by failed directory sync must not release this version's data.
        self.retained_catalog.insert(offset, capture.catalog_pin.clone());
        let mut history = HistorySeal {
            id: self.current.id,
            store: &self.current.store,
            index: &self.current.index,
            pins: &mut self.pins,
        };
        match (*capture).complete(&self.cfg, &mut history, &self.key, &self.lifecycle) {
            Ok(()) => {
                // The pair's history certificate is a signed head at this checkpoint.
                let head = self.current.index.head();
                self.certified_seq = head.last_seq();
                self.last_certified = Some(head.root);
            }
            Err(error) => warn!(alert = "snapshot_write_failed", seq = offset.seq, "keeping earlier caches: {error:#}"),
        }
        self.refresh_pins();
    }
}
