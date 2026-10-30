//! The sequencer's end of the derived marks cache: it reports each market's mark changes to the
//! cache's task, at the command's sequenced time, so a cache seeded at recovery continues exactly.

use engine_core::types::Millis;
use tokio::sync::mpsc;

use crate::history::marks::MarkSample;
use crate::sequencer::Sequencer;

impl Sequencer {
    /// Sends each market's mark changes to `tx` for the derived marks cache. Only changes go, at
    /// the command's sequenced time, so a cache seeded at recovery continues exactly.
    pub fn track_marks(&mut self, tx: mpsc::Sender<MarkSample>) {
        self.marks_tx = Some(tx);
    }

    /// Reports each listed market whose mark moved on the command just applied, in market order,
    /// at its sequenced time. A full queue drops the sample: the cache is derived, and the next
    /// change or a recovery reseeds it.
    pub(super) fn report_marks(&mut self, ts: Millis) {
        let Some(tx) = &self.marks_tx else { return };
        for (id, market) in self.engine.markets() {
            if let Some(mark) = market.mark
                && self.last_marks.insert(*id, mark) != Some(mark)
            {
                let _ = tx.try_send((*id, ts, mark));
            }
        }
    }
}
