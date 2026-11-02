//! The sequencer's end of the derived marks cache: it reports each market's mark changes, once
//! the command that produced them is durable in the log, to the cache's task at the command's
//! sequenced time, so a cache seeded at recovery continues exactly.

use std::collections::BTreeMap;

use engine_core::types::{MarketId, Millis};
use engine_core::{Engine, Fixed};
use tokio::sync::mpsc::{self, error::TrySendError};
use tracing::{debug, info, warn};

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
    /// The dashboard record every applied command is added to, once the node attaches it.
    stats: Option<crate::stats::Stats>,
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
    /// says so, and the first batch the queue takes whole ends the episode.
    fn release(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let Some(tx) = &self.tx else {
            self.pending.clear();
            return;
        };
        let mut dropped = false;
        let mut closed = false;
        for sample in self.pending.drain(..) {
            match tx.try_send(sample) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => dropped = true,
                Err(TrySendError::Closed(_)) => closed = true,
            }
        }
        // The cache task ended (a shutdown, which it flushes through, or a failure its supervisor
        // stops the node for): nothing is lost to a full queue, so no alert, and no more sends.
        if closed {
            debug!("the marks task has ended; marks reports stop");
            self.tx = None;
            return;
        }
        if dropped && !self.overflow_warned {
            warn!(
                alert = "marks_samples_dropped",
                "the marks queue is full; dropped samples reseed at the next recovery"
            );
            self.overflow_warned = true;
        } else if !dropped && self.overflow_warned {
            info!(alert_resolved = "marks_samples_dropped", "the marks queue takes samples again");
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

    /// Buffers the mark changes of the command just applied for release with this batch's flush, and
    /// adds the command, which produced `events`, to the dashboard record.
    pub(super) fn report_marks(&mut self, ts: Millis, events: &[engine_core::event::Event]) {
        self.marks.report(&self.engine, ts);
        if let Some(stats) = &self.marks.stats {
            stats.observe(&self.engine, ts, events);
        }
    }

    /// Releases buffered mark reports once their commands are durable in the log.
    pub(super) fn release_marks(&mut self) {
        self.marks.release();
    }
}

impl Sequencer {
    /// Adds every command this sequencer applies to the dashboard record `stats`, which recovery
    /// may have seeded, and gives it the recovered state's scan if its replay took none.
    pub fn track_stats(&mut self, stats: crate::stats::Stats) {
        stats.finish_recovery(&self.engine);
        self.marks.stats = Some(stats);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks_with(capacity: usize) -> (Marks, mpsc::Receiver<MarkSample>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Marks { tx: Some(tx), ..Marks::default() }, rx)
    }

    #[test]
    fn a_full_queue_drops_samples_and_a_batch_it_takes_whole_ends_the_episode() {
        let (mut marks, mut rx) = marks_with(1);
        marks.pending = vec![(1, 10, Fixed::from_int(1)), (2, 10, Fixed::from_int(2))];
        marks.release();
        assert!(marks.overflow_warned, "the second sample found the queue full");
        assert_eq!(rx.try_recv().unwrap().0, 1);
        assert!(rx.try_recv().is_err(), "the dropped sample is not queued");
        // A later batch the queue takes whole clears the episode, so the next overflow alerts again.
        marks.pending = vec![(1, 20, Fixed::from_int(3))];
        marks.release();
        assert!(!marks.overflow_warned);
        assert_eq!(rx.try_recv().unwrap().2, Fixed::from_int(3));
    }

    #[test]
    fn a_closed_queue_is_not_a_drop_and_stops_reporting() {
        let (mut marks, rx) = marks_with(1);
        drop(rx);
        marks.pending = vec![(1, 10, Fixed::from_int(1))];
        marks.release();
        assert!(!marks.overflow_warned, "a receiver gone at shutdown is not an overflow");
        assert!(marks.tx.is_none());
        assert!(marks.pending.is_empty());
    }

    #[test]
    fn an_episode_stays_open_while_the_queue_keeps_dropping() {
        let (mut marks, _rx) = marks_with(1);
        marks.pending = vec![(1, 10, Fixed::from_int(1)), (2, 10, Fixed::from_int(2))];
        marks.release();
        marks.pending = vec![(1, 20, Fixed::from_int(3))];
        marks.release();
        assert!(marks.overflow_warned, "the queue never drained");
    }
}
