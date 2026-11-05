//! The daily turns the log assigned, held in a bounded ring: the history `GET /v1/operator` pages.

use std::collections::VecDeque;

use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::turns::TurnReason;
use engine_core::types::{AccountId, Millis};
use serde::Serialize;

use crate::stats::Stats;

/// Most turns kept: about three years of daily turns, with their reassignments.
pub(crate) const TURN_HISTORY: usize = 2_000;
/// Most turns per page.
pub(crate) const MAX_TURNS_PAGE: usize = 100;

/// One assignment of a turn, as its mirror's events said, and its member's `TurnTaken`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TurnRecord {
    pub turn: u64,
    pub position: u16,
    pub reason: TurnReason,
    pub candidate: AccountId,
    pub admin_key: AccountId,
    /// The seq and sequenced time of the command that mirrored it.
    pub assigned_seq: u64,
    pub assigned_ms: Millis,
    /// When its key signs from; `None` for a turn the engine refused.
    pub applies_at: Option<Millis>,
    /// Why the engine refused it, keeping the key in effect.
    pub refused: Option<String>,
    /// The `TurnTaken` that started its member's time in office, and the release that logged it.
    pub taken_seq: Option<u64>,
    pub taken_ms: Option<Millis>,
    pub release: Option<B256>,
}

/// The newest `TURN_HISTORY` turn assignments, oldest first.
#[derive(Default)]
pub(crate) struct TurnHistory {
    records: VecDeque<TurnRecord>,
    /// Whether the ring has dropped a record for room.
    pub(crate) evicted: bool,
}

impl TurnHistory {
    /// Records what one command's `events` say about turns, at `seq` and `ts`.
    pub(crate) fn observe(&mut self, seq: u64, ts: Millis, events: &[Event]) {
        for event in events {
            let record = |turn, position, reason, candidate, admin_key, applies_at, refused| TurnRecord {
                turn,
                position,
                reason,
                candidate,
                admin_key,
                assigned_seq: seq,
                assigned_ms: ts,
                applies_at,
                refused,
                taken_seq: None,
                taken_ms: None,
                release: None,
            };
            match event {
                Event::TurnAssigned { turn, position, reason, candidate, admin_key, applies_at } => {
                    self.push(record(*turn, *position, *reason, *candidate, *admin_key, Some(*applies_at), None))
                }
                Event::TurnRefused { turn, position, cause, candidate, admin_key, reason } => {
                    self.push(record(*turn, *position, *cause, *candidate, *admin_key, None, Some(reason.clone())))
                }
                Event::TurnTaken { turn, candidate, release } => {
                    let taken = self.records.iter_mut().rev().find(|record| {
                        record.turn == *turn && record.candidate == *candidate && record.refused.is_none()
                    });
                    if let Some(taken) = taken {
                        (taken.taken_seq, taken.taken_ms, taken.release) = (Some(seq), Some(ts), Some(*release));
                    }
                }
                _ => {}
            }
        }
    }

    /// Adds the newest record, dropping the oldest past capacity.
    fn push(&mut self, record: TurnRecord) {
        if self.records.len() == TURN_HISTORY {
            self.records.pop_front();
            self.evicted = true;
        }
        self.records.push_back(record);
    }

    /// Up to `limit` records mirrored before seq `before`, newest first, and the seq to pass as
    /// `before` for the page after them when more are held. A turn's reassignment can follow the
    /// next turn's draw, so records are held, and paged, in the order they were mirrored.
    pub(crate) fn page(&self, before: Option<u64>, limit: usize) -> (Vec<TurnRecord>, Option<u64>) {
        let end = before
            .map_or(self.records.len(), |before| self.records.partition_point(|record| record.assigned_seq < before));
        let mut older = self.records.range(..end).rev();
        let page: Vec<TurnRecord> = older.by_ref().take(limit).cloned().collect();
        let next = older.next().and(page.last()).map(|last| last.assigned_seq);
        (page, next)
    }

    /// The seq of the oldest record held.
    pub(crate) fn oldest_seq(&self) -> Option<u64> {
        self.records.front().map(|record| record.assigned_seq)
    }
}

/// A page of the turn history, newest first.
#[derive(Debug, Serialize)]
pub(crate) struct TurnsPage {
    pub records: Vec<TurnRecord>,
    /// The `before` for the page after this one; `None` when none is held.
    pub next: Option<u64>,
    /// Whether the ring has dropped older records for room, and the seq of the oldest it holds: a
    /// page that ends with `next` null is then the end of what is held, not of what happened.
    pub truncated: bool,
    pub retained_from_seq: Option<u64>,
}

impl Stats {
    /// The turn assignments mirrored before seq `before`, newest first.
    /// Error cases:
    /// - the limit is outside 1 to `MAX_TURNS_PAGE`
    pub(crate) fn turns(&self, before: Option<u64>, limit: usize) -> Result<TurnsPage, String> {
        if !(1..=MAX_TURNS_PAGE).contains(&limit) {
            return Err(format!("limit must be from 1 to {MAX_TURNS_PAGE}"));
        }
        let inner = self.lock();
        let (records, next) = inner.turns.page(before, limit);
        Ok(TurnsPage { records, next, truncated: inner.turns.evicted, retained_from_seq: inner.turns.oldest_seq() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMBER: AccountId = AccountId([0xc1; 20]);
    const KEY: AccountId = AccountId([0xa1; 20]);

    fn assigned(turn: u64, position: u16, reason: TurnReason) -> Event {
        Event::TurnAssigned {
            turn,
            position,
            reason,
            candidate: MEMBER,
            admin_key: KEY,
            applies_at: 1_000 * turn as i64,
        }
    }

    #[test]
    fn each_assignment_is_a_record_its_turn_taken_completes_and_pages_go_back_by_seq() {
        let mut history = TurnHistory::default();
        history.observe(10, 100, &[assigned(5, 0, TurnReason::Draw)]);
        history.observe(11, 110, &[assigned(6, 0, TurnReason::Draw)]);
        // A reassignment of turn 5 after turn 6 was drawn stays in mirror order.
        history.observe(12, 120, &[assigned(5, 1, TurnReason::Promotion)]);
        let refused = Event::TurnRefused {
            turn: 7,
            position: 0,
            cause: TurnReason::Draw,
            candidate: MEMBER,
            admin_key: KEY,
            reason: "no notice".into(),
        };
        history.observe(13, 130, &[refused]);
        let release = B256::repeat_byte(3);
        history.observe(14, 140, &[Event::TurnTaken { turn: 5, candidate: MEMBER, release }]);

        let (page, next) = history.page(None, 2);
        assert_eq!(page.iter().map(|r| (r.turn, r.assigned_seq)).collect::<Vec<_>>(), [(7, 13), (5, 12)]);
        assert_eq!(page[0].refused.as_deref(), Some("no notice"));
        assert_eq!(page[0].applies_at, None);
        assert_eq!((page[1].taken_seq, page[1].taken_ms, page[1].release), (Some(14), Some(140), Some(release)));
        assert_eq!(next, Some(12));
        let (page, next) = history.page(next, 2);
        assert_eq!(page.iter().map(|r| (r.turn, r.assigned_seq)).collect::<Vec<_>>(), [(6, 11), (5, 10)]);
        assert_eq!(page[1].taken_seq, None, "the earlier assignment of the turn was not the one taken");
        assert_eq!(next, None);
        assert_eq!(history.oldest_seq(), Some(10));
    }

    #[test]
    fn the_ring_drops_its_oldest_record_past_capacity() {
        let mut history = TurnHistory::default();
        for seq in 0..=TURN_HISTORY as u64 {
            history.observe(seq, 0, &[assigned(seq, 0, TurnReason::Draw)]);
        }
        assert!(history.evicted);
        assert_eq!(history.oldest_seq(), Some(1));
    }
}
