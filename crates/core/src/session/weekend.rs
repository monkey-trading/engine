//! Weekend gaps: the closures of at least `WEEKEND_GAP_MS` a market's weekend margin covers. A daily
//! break or an overnight close is shorter; a weekend is longer, and a holiday next to it, or an early
//! close before it, makes it start earlier or end later.

use chrono::TimeZone;

use crate::session::{MS_PER_MINUTE, Schedule};
use crate::types::Millis;

/// The least a closure lasts to be a weekend gap: 40 hours.
pub const WEEKEND_GAP_MS: Millis = 40 * 60 * 60 * 1000;
/// How far the gap searches look: 366 days. A gap that has not reopened by then counts as reopening
/// then, where it is looked at again.
pub const WEEKEND_SEARCH_MS: Millis = 366 * 24 * 60 * 60 * 1000;

impl Schedule {
    /// The first weekend gap that starts after `after` and within `WEEKEND_SEARCH_MS` of it: the
    /// instant the venue shuts for a closure of at least `WEEKEND_GAP_MS`, and the instant it opens
    /// again, or `WEEKEND_SEARCH_MS` after it shut if that is sooner. `None` for a schedule that is
    /// always open, or one with no such closure starting in that time.
    pub fn next_weekend_gap(&self, after: Millis) -> Option<(Millis, Millis)> {
        let limit = after.saturating_add(WEEKEND_SEARCH_MS);
        let mut from = after;
        loop {
            let opens = if self.is_open(from) { from } else { self.next_change(from, limit)? };
            let close = self.next_change(opens, limit)?;
            match self.next_change(close, close.saturating_add(WEEKEND_GAP_MS)) {
                Some(reopens) if reopens - close < WEEKEND_GAP_MS => from = reopens,
                _ => return Some((close, self.reopening(close))),
            }
        }
    }

    /// When the venue, shut at `at` inside a closure of at least `WEEKEND_GAP_MS`, opens again, or
    /// `WEEKEND_SEARCH_MS` after `at` if that is sooner; `None` while it is open or in a shorter
    /// closure. The closure is that long when the venue is shut throughout the `WEEKEND_GAP_MS` before
    /// it reopens, which needs checking only from that point to `at`, so its start need not be found.
    pub fn weekend_gap_at(&self, at: Millis) -> Option<Millis> {
        if self.is_open(at) {
            return None;
        }
        let reopen = self.reopening(at);
        let from = reopen - WEEKEND_GAP_MS;
        let long = at < from || (!self.is_open(from) && self.next_change(from, at).is_none());
        long.then_some(reopen)
    }

    /// When the venue, shut at `at`, opens again, or `WEEKEND_SEARCH_MS` after `at` if that is sooner.
    fn reopening(&self, at: Millis) -> Millis {
        let limit = at.saturating_add(WEEKEND_SEARCH_MS);
        self.next_change(at, limit).unwrap_or(limit)
    }

    /// The first instant after `after`, and no later than `limit`, at which the venue is open if it
    /// is shut at `after`, or shut if it is open; `None` if there is none. Openness changes only at
    /// a minute boundary or at a dated period's start or end, each local time that occurs twice as
    /// the clocks go back at both instants, taken at the first millisecond at or after it (see
    /// `Schedule::open_throughout`), so each of those is checked in turn; each minute costs a check,
    /// so callers bound the span.
    fn next_change(&self, after: Millis, limit: Millis) -> Option<Millis> {
        let Schedule::Weekly { tz, closures, special_sessions, .. } = self else { return None };
        let open = self.is_open(after);
        let changes = |at: &Millis| self.is_open(*at) != open;
        let first_millisecond = |at: chrono::DateTime<_>| {
            at.timestamp_millis() + i64::from(!at.timestamp_subsec_nanos().is_multiple_of(1_000_000))
        };
        let mut edges: Vec<Millis> = closures
            .iter()
            .chain(special_sessions)
            .flat_map(|period| [period.from, period.to])
            .flat_map(|edge| {
                let local = tz.from_local_datetime(&edge);
                [local.earliest(), local.latest()]
            })
            .flatten()
            .map(first_millisecond)
            .filter(|at| after < *at && *at <= limit)
            .collect();
        edges.sort_unstable();
        edges.dedup();
        let mut edges = edges.into_iter().peekable();
        let minutes =
            (after.div_euclid(MS_PER_MINUTE) + 1..=limit.div_euclid(MS_PER_MINUTE)).map(|m| m * MS_PER_MINUTE);
        for minute in minutes {
            while let Some(edge) = edges.next_if(|edge| *edge < minute) {
                if changes(&edge) {
                    return Some(edge);
                }
            }
            if changes(&minute) {
                return Some(minute);
            }
        }
        edges.find(changes)
    }
}
