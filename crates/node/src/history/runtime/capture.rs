//! Completing a checkpoint capture into the paired cache, and the alert its failures raise.

use std::fmt::Display;
use std::sync::atomic::Ordering;

use tracing::{info, warn, warn_span};

use crate::history::runtime::Actor;
use crate::snapshot::sealing::{Capture, HistorySeal};

impl Actor {
    /// Publishes the paired cache for a capture whose checkpoint is the last entry taken, `taken`
    /// when that entry is a checkpoint; any other entry drops the capture.
    pub(super) fn complete_capture(&mut self, capture: Box<Capture>, taken: Option<&crate::log::Checkpoint>) {
        let offset = capture.snapshot.offset;
        if self.last_entry != Some(offset) || taken != Some(&capture.snapshot.checkpoint) {
            self.capture_failed(offset.seq, "history did not observe the exact captured checkpoint");
            return;
        }
        // A rename followed by failed directory sync must not release this version's data.
        self.retained_catalog.insert(offset, capture.catalog_pin.clone());
        // Taken while this capture holds the sequencer's only capture permit, so a sequencer failure
        // can only follow the take, and stays set for the next stored capture.
        let resolves = self.write_alert_open.swap(false, Ordering::AcqRel);
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
                if resolves {
                    info!(alert_resolved = "snapshot_write_failed", "a checkpoint snapshot is stored");
                }
            }
            Err(error) => self.capture_failed(offset.seq, format_args!("keeping earlier caches: {error:#}")),
        }
        self.refresh_pins();
    }

    /// Raises `snapshot_write_failed` for the capture of the checkpoint at `seq`, which the next
    /// stored capture resolves. The seq is span context, not an event field, because alert fields
    /// key episodes: failures at successive checkpoints stay one episode.
    pub(super) fn capture_failed(&self, seq: u64, reason: impl Display) {
        self.write_alert_open.store(true, Ordering::Release);
        warn_span!("capture", seq).in_scope(|| warn!(alert = "snapshot_write_failed", "{reason}"));
    }
}
