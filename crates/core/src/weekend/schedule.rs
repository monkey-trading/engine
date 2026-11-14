//! When each market's weekend gap is: set by a listing or `V59`'s activation, and moved on at each
//! reopening; and the checks a listing's weekend margin and calendar pass. A search that finds no gap
//! keeps a frontier in its place, where it runs again (`WeekendGap::is_frontier`).

use std::collections::BTreeSet;

use crate::engine::Engine;
use crate::event::Event;
use crate::market::MarketSpec;
use crate::operator_bounds::NOTICE_MS;
use crate::protocol::ProtocolVersion;
use crate::session::{Schedule, WEEKEND_GAP_MS, WEEKEND_SEARCH_MS};
use crate::types::{MarketId, Millis};
use crate::weekend::{WEEKEND_LEAD_MS, WeekendGap};

impl Engine {
    /// From `V59`, after a listing applies, sets `market`'s weekend gap under its calendar: the gap it
    /// is in now, if any, and the next one otherwise. A gap the market is in that the gap kept before
    /// had not closed out is closed out now, at the marks; one it had keeps its close-out and takes the
    /// calendar's reopening. A gap yet to start whose close-out the kept gap had waived, as an
    /// activation does, stays waived. A frontier kept before is no gap. A market with no weekend margin
    /// keeps no gap.
    pub(crate) fn relist_weekend(&mut self, market: MarketId, events: &mut Vec<Event>) {
        let now = self.now;
        let state = &self.markets[&market];
        let kept = state.weekend;
        let (gap, close_out) = if self.protocol_version < ProtocolVersion::V59 || state.spec.weekend_margin_bps == 0 {
            (None, false)
        } else if let Some(reopen) = state.spec.schedule.weekend_gap_at(now) {
            let ran = kept.filter(|gap| gap.closed_out && !gap.is_frontier() && now < gap.reopen);
            let close = ran.map_or(now, |gap| gap.close.min(now));
            (Some(WeekendGap { close, reopen, closed_out: true }), ran.is_none())
        } else {
            let next = next_gap(&state.spec.schedule, now);
            let waived = kept
                .is_some_and(|gap| gap.closed_out && !gap.is_frontier() && now < gap.close && next.close < gap.reopen);
            (Some(WeekendGap { closed_out: next.closed_out || waived, ..next }), false)
        };
        if kept != gap {
            self.markets.get_mut(&market).expect("listed").weekend = gap;
        }
        if close_out {
            self.weekend_close_outs(&BTreeSet::from([market]), now, events);
        }
    }

    /// At a closed-out weekend gap's reopening `at`: a market still shut in the gap, past the end of
    /// a search, stays in it to its next reopening; one that has reopened, or whose gap was a
    /// frontier, takes its next gap. A frontier's search found every closure of at least
    /// `WEEKEND_GAP_MS` starting before it, so none is under way there. A next gap starting within
    /// `WEEKEND_LEAD_MS` of the closed-out gap's recorded start is held with no close-out: a gap of
    /// the calendar lasts at least `WEEKEND_GAP_MS`, so only one an activation or a listing entered,
    /// recorded from then, has one so close, whose holders would otherwise have less than the lead.
    pub(crate) fn move_weekend_gap_on(&mut self, market: MarketId, at: Millis) {
        let state = self.markets.get_mut(&market).expect("listed");
        let schedule = &state.spec.schedule;
        state.weekend = Some(match (schedule.weekend_gap_at(at), state.weekend) {
            (Some(reopen), Some(gap)) if !gap.is_frontier() => WeekendGap { reopen, ..gap },
            (_, kept) => {
                let next = next_gap(schedule, at);
                let entered = kept.filter(|gap| !gap.is_frontier()).map_or(Millis::MIN, |gap| gap.close);
                let waived = next.close < entered.saturating_add(WEEKEND_LEAD_MS);
                WeekendGap { closed_out: next.closed_out || waived, ..next }
            }
        });
    }

    /// On an activation carrying `version` to `V59` or later from before it: sets the weekend gap of
    /// each market listed with a weekend margin, which applies from then: the gap it is in now, held
    /// to its reopening with no close-out, or its next, held the same way when it starts within
    /// `WEEKEND_LEAD_MS`, which would leave positions opened under the version before less than the
    /// lead.
    pub(crate) fn start_weekend_closes(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V59 || version < ProtocolVersion::V59 {
            return;
        }
        let now = self.now;
        let ids: Vec<MarketId> =
            self.markets.iter().filter(|(_, market)| market.spec.weekend_margin_bps != 0).map(|(id, _)| *id).collect();
        for id in ids {
            let market = self.markets.get_mut(&id).expect("listed");
            let schedule = &market.spec.schedule;
            market.weekend = Some(match schedule.weekend_gap_at(now) {
                Some(reopen) => WeekendGap { close: now, reopen, closed_out: true },
                None => match next_gap(schedule, now) {
                    gap if !gap.is_frontier() && gap.close < now + WEEKEND_LEAD_MS => {
                        WeekendGap { closed_out: true, ..gap }
                    }
                    gap => gap,
                },
            });
        }
    }

    /// Checks a listing's weekend margin and calendar from `V59`. A weekend margin needs a calendar
    /// with a weekend gap now or starting within `WEEKEND_SEARCH_MS`, so it applies. Unless `voted`, a
    /// relisting with a weekend margin may change a listed market's weekend gaps only as its holders
    /// could see coming, as a calendar change could otherwise do with no notice. The new calendar must
    /// keep the market shut, in a weekend gap, through each weekend gap the calendar in force gives
    /// that has started or starts within `NOTICE_MS`, from its start, or now once it has started, to
    /// its reopening: no such gap is narrowed or removed sooner than the rate itself could be, held or
    /// not, since new positions could otherwise carry it at the normal margin. And while the market
    /// holds positions, each weekend gap the new calendar has started or starts within `NOTICE_MS`, a
    /// gap in progress counting as starting now, must be one the calendar in force has the market in
    /// then: a new or earlier gap would close holders out at its weekend margin, a margin raise with
    /// less notice than a rate change takes.
    /// Before `V59` a weekend margin does nothing, as a field a later version reads.
    /// Error cases:
    /// - from `V59`, a weekend margin on a calendar with no closure of at least `WEEKEND_GAP_MS` now
    ///   or starting within `WEEKEND_SEARCH_MS` of the sequenced time
    /// - from `V59`, unless `voted`, a relisting of a listed market with a weekend margin whose
    ///   calendar narrows or removes a weekend gap starting within `NOTICE_MS`, or, while the market
    ///   holds positions, starts one within `NOTICE_MS` that the calendar in force does not
    pub(crate) fn check_weekend_listing(&self, spec: &MarketSpec, voted: bool) -> Result<(), String> {
        if spec.weekend_margin_bps == 0 || self.protocol_version < ProtocolVersion::V59 {
            return Ok(());
        }
        let (now, schedule) = (self.now, &spec.schedule);
        if schedule.weekend_gap_at(now).is_none() && schedule.next_weekend_gap(now).is_none() {
            return Err(format!(
                "market {}'s calendar has no closure of at least {} hours starting within {} days, so its weekend margin would never apply",
                spec.id,
                WEEKEND_GAP_MS / 3_600_000,
                WEEKEND_SEARCH_MS / 86_400_000
            ));
        }
        let Some(market) = self.markets.get(&spec.id).filter(|_| !voted) else {
            return Ok(());
        };
        let in_force = &market.spec.schedule;
        for (start, reopen) in gaps_within(in_force, now, now + NOTICE_MS) {
            if schedule.weekend_gap_at(start).is_none_or(|until| until < reopen) {
                return Err(format!(
                    "a calendar change may not narrow or remove market {}'s weekend gap from {start} to {reopen}, within {} days",
                    spec.id,
                    NOTICE_MS / 86_400_000
                ));
            }
        }
        let holders = &self.hot_indexes.as_ref().expect("apply builds derived indexes").holders;
        if holders.get(&spec.id).is_none_or(|accounts| accounts.is_empty()) {
            return Ok(());
        }
        for (start, _) in gaps_within(schedule, now, now + NOTICE_MS) {
            if in_force.weekend_gap_at(start).is_none() {
                return Err(format!(
                    "market {} holds positions, so a calendar change may not start a weekend gap at {start}, within {} days, that its calendar does not",
                    spec.id,
                    NOTICE_MS / 86_400_000
                ));
            }
        }
        Ok(())
    }
}

/// The weekend gaps `schedule` has the market in at `now`, starting then, and those starting
/// before `until`, each with its reopening.
fn gaps_within(schedule: &Schedule, now: Millis, until: Millis) -> Vec<(Millis, Millis)> {
    let mut gaps: Vec<(Millis, Millis)> =
        schedule.weekend_gap_at(now).map(|reopen| (now, reopen)).into_iter().collect();
    let mut after = gaps.last().map_or(now, |(_, reopen)| *reopen);
    while let Some((close, reopen)) = schedule.next_weekend_gap(after).filter(|(close, _)| *close < until) {
        gaps.push((close, reopen));
        after = reopen;
    }
    gaps
}

/// The next weekend gap `schedule` gives after `after`, its close-out still to run, or, when none
/// starts within `WEEKEND_SEARCH_MS`, a frontier where the search runs again: `WEEKEND_LEAD_MS`
/// before the search's end, so a gap starting just past it is found before its lead begins.
fn next_gap(schedule: &Schedule, after: Millis) -> WeekendGap {
    match schedule.next_weekend_gap(after) {
        Some((close, reopen)) => WeekendGap { close, reopen, closed_out: false },
        None => {
            let end = after.saturating_add(WEEKEND_SEARCH_MS - WEEKEND_LEAD_MS);
            WeekendGap { close: end, reopen: end, closed_out: true }
        }
    }
}
