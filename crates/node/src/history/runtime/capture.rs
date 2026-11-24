//! Completing a checkpoint capture into the paired cache, at its checkpoint or, for an owner behind its
//! input, before it, and the alert its failures raise.

use std::fmt::Display;

use anyhow::{Result, ensure};
use tracing::{warn, warn_span};

use crate::history::runtime::Actor;
use crate::snapshot::sealing::{Capture, HistorySeal};

impl Actor {
    /// Holds `capture` until the owner takes its checkpoint, or until a full turn shows the input running
    /// ahead of the owner (`complete_behind`), or completes it at once when the owner stands at its
    /// checkpoint.
    /// Error cases:
    /// - another capture is in flight, or indexing the turn's commands fails
    pub(super) fn seal(&mut self, capture: Box<Capture>) -> Result<()> {
        ensure!(self.pending.is_none(), "more than one in-flight history capture");
        if let Err(error) = self.settle_pins() {
            warn!(alert = "snapshot_cleanup_failed", "skipping another optional capture: {error:#}");
        } else if self.last_entry == Some(capture.snapshot.offset) {
            // Shutdown's capture of the checkpoint this owner has just taken: the index stands exactly
            // at it once the turn's commands are indexed.
            self.index_commands()?;
            let checkpoint = self.at_checkpoint.clone();
            self.complete_capture(capture, checkpoint.as_ref());
        } else if self.last_entry.is_some_and(|last| last > capture.snapshot.offset) {
            self.capture_failed(capture.snapshot.offset.seq, "optional checkpoint fence was already crossed");
        } else {
            self.pending = Some(capture);
        }
        Ok(())
    }

    /// Publishes the paired cache for a capture whose checkpoint is the last entry taken, `taken`
    /// when that entry is a checkpoint, with the history indexed by then; any other entry drops the
    /// capture.
    pub(super) fn complete_capture(&mut self, capture: Box<Capture>, taken: Option<&crate::log::Checkpoint>) {
        let offset = capture.snapshot.offset;
        if self.last_entry != Some(offset) || taken != Some(&capture.snapshot.checkpoint) {
            self.capture_failed(offset.seq, "history did not observe the exact captured checkpoint");
            return;
        }
        self.store_capture(capture);
    }

    /// Publishes the paired cache for the capture waiting for its checkpoint, if any, with the history
    /// indexed so far, which is before its checkpoint: for an owner its input runs ahead of, whose backlog
    /// the capture would otherwise wait for. An owner that has indexed no command keeps it waiting.
    pub(super) fn complete_behind(&mut self) {
        if self.current.index.head().last_seq().is_none() {
            return;
        }
        if let Some(capture) = self.pending.take() {
            self.store_capture(capture);
        }
    }

    /// Publishes the paired cache for `capture` with the history indexed so far, at or before its
    /// checkpoint, which is durable in the log: the sequencer syncs the log before it hands a capture over.
    fn store_capture(&mut self, capture: Box<Capture>) {
        let offset = capture.snapshot.offset;
        // A rename followed by failed directory sync must not release this version's data.
        self.retained_catalog.insert(offset, capture.catalog_pin.clone());
        // `stored` resolves only failures raised before this take; one raised later, such as the
        // shutdown wait's, stays open for the next stored capture.
        let taken = self.write_alert.take();
        let mut history = HistorySeal {
            id: self.current.id,
            store: &self.current.store,
            index: &self.current.index,
            pins: &mut self.pins,
        };
        match (*capture).complete(&self.cfg, &mut history, (&self.key, &self.lifecycle), &self.stopped) {
            Ok(()) => {
                // The pair's history certificate is a signed head at the history it took.
                let head = self.current.index.head();
                self.certified_seq = head.last_seq();
                self.last_certified = Some(head.root);
                self.write_alert.stored(taken);
            }
            Err(error) => self.capture_failed(offset.seq, format_args!("keeping earlier caches: {error:#}")),
        }
        self.refresh_pins();
    }

    /// Raises `snapshot_write_failed` for the capture of the checkpoint at `seq`, which the next
    /// stored capture resolves. The seq is span context, not an event field, because alert fields
    /// key episodes: failures at successive checkpoints stay one episode.
    pub(super) fn capture_failed(&self, seq: u64, reason: impl Display) {
        self.write_alert
            .failed(|| warn_span!("capture", seq).in_scope(|| warn!(alert = "snapshot_write_failed", "{reason}")));
    }
}
