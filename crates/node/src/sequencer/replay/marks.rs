//! Replay's marks sink: hands each market's mark, as it changes, to the cache seeder recovery
//! replays through, in market order at each command's sequenced time.

use std::collections::BTreeMap;

use engine_core::types::{MarketId, Millis};
use engine_core::{Engine, Fixed};

use crate::sequencer::replay::Replay;

impl Replay {
    /// Sets the sink that takes each market's mark as it changes during this replay, which recovery
    /// uses to seed the derived marks cache.
    pub(crate) fn sample_marks_with(&mut self, sink: crate::history::marks::MarkSink) {
        self.marks = Some(sink);
    }
}

/// Hands each market whose mark this command moved to the marks sink, in market order, at the
/// command's sequenced time; unchanged marks are not re-taken, so a live cache seeded this way
/// continues a recovery-seeded one exactly.
pub(crate) fn sample_marks(
    sink: &mut Option<crate::history::marks::MarkSink>,
    last_marks: &mut BTreeMap<MarketId, Fixed>,
    engine: &Engine,
    ts: Millis,
) {
    let Some(sink) = sink.as_mut() else { return };
    for (id, market) in engine.markets() {
        if let Some(mark) = market.mark
            && last_marks.insert(*id, mark) != Some(mark)
        {
            sink(*id, ts, mark);
        }
    }
}
