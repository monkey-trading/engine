//! The sequencer's end of the derived marks cache: it reports each market's mark changes, once
//! the command that produced them is durable in the log, to the cache's task at the command's
//! sequenced time, so a cache seeded at recovery continues exactly.

use std::collections::BTreeMap;

use engine_core::types::{MarketId, Millis};
use engine_core::{Engine, Fixed};
use tokio::sync::mpsc;
use tracing::warn;

use crate::history::marks::MarkSample;
use crate::sequencer::Sequencer;

/// Live mark reporting for the derived marks cache: the channel, the last mark per market (only
/// changes are reported, and recovery hands its map over so reporting continues exactly where the
/// replay left off), reports pending their command's durability, and one overflow alert per
/// episode.
#[derive(Default)]
pub(super) struct Marks {
    tx: Option<mpsc::Sender<MarkSample>>,
    last: BTreeMap<MarketId, Fixed>,
    pending: Vec<MarkSample>,
    overflow_warned: bool,
}

impl Marks {
    /// Continues change-only reporting from `last`, the map recovery's replay sampled.
    pub fn take_recovered(&mut self, last: BTreeMap<MarketId, Fixed>) {
        self.last = last;
    }

    /// Buffers each listed market whose mark moved on the command just applied, in market order,
    /// at its sequenced time, for `release` once the command is durable in the log.
    fn report(&mut self, engine: &Engine, ts: Millis) {
        for (id, market) in engine.markets() {
            if let Some(mark) = market.mark
                && self.last.insert(*id, mark) != Some(mark)
            {
                self.pending.push((*id, ts, mark));
            }
        }
    }

    /// Sends the buffered reports once their commands are durable. A full queue drops the sample:
    /// the cache is derived, and the next change or a recovery reseeds it; one alert per episode
    /// says so.
    fn release(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let Some(tx) = &self.tx else {
            self.pending.clear();
            return;
        };
        let mut dropped = false;
        for sample in self.pending.drain(..) {
            dropped |= tx.try_send(sample).is_err();
        }
        if dropped && !self.overflow_warned {
            warn!(
                alert = "marks_samples_dropped",
                "the marks queue is full; dropped samples reseed at the next recovery"
            );
            self.overflow_warned = true;
        } else if !dropped {
            self.overflow_warned = false;
        }
    }
}

impl Sequencer {
    /// Sends each market's mark changes to `tx` for the derived marks cache. Only changes go, at
    /// the command's sequenced time, once the command is durable, so a cache seeded at recovery
    /// continues exactly.
    pub fn track_marks(&mut self, tx: mpsc::Sender<MarkSample>) {
        self.marks.tx = Some(tx);
    }

    /// Buffers the mark changes of the command just applied for release with this batch's flush.
    pub(super) fn report_marks(&mut self, ts: Millis) {
        self.marks.report(&self.engine, ts);
    }

    /// Releases buffered mark reports once their commands are durable in the log.
    pub(super) fn release_marks(&mut self) {
        self.marks.release();
    }
}
