//! Each market's markout spread, the toxicity score's unit: the root mean square of its index's 5 s
//! changes over the trailing hour, never below the floor its listing names.
//!
//! Sequenced time is cut into a grid of `GRID_MS` points. Each command samples every grid point from
//! the previous command's time up to but not including its own, from the market as the commands
//! before it left it: its index at that point, when it is in session, not blocked by a corporate
//! action, and its quotes give one, and has been open since the previous command, as its quotes are
//! from then or before and a closure between ends them. Every grid point is sampled once, by the
//! first command after it. A change counts when the market had an index at both ends of an interval
//! and was open throughout it, and had one just before and just after each price command and each
//! relisting of it within the interval, the commands that change what prices it. Between them its
//! quotes only age. From `V58` a change also needs an index at each moment within the interval at
//! which one of the market's quotes aged out, since where three or more venues quote, ageing alone can
//! end its index and bring it back; before `V58` those moments are not checked. Each counted change
//! is kept, as its square in bps of the earlier index, for the trailing hour. A split needs a venue
//! close before its ex-date open, so no change spans it, and the changes kept are relative, so a
//! split leaves the history as it is.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::{Engine, MarketState};
use crate::event::Event;
use crate::fixed::Rounding;
use crate::markouts::MAX_MARKOUT_BPS;
use crate::price::SourceQuote;
use crate::protocol::ProtocolVersion;
use crate::types::{MarketId, Millis};

/// The spacing of the sampling grid: the markout horizon.
pub const GRID_MS: Millis = 5_000;
/// The trailing window the markout spread reads: an hour.
pub const WINDOW_MS: Millis = 60 * 60 * 1000;
/// The intervals the trailing window holds.
const WINDOW: i64 = WINDOW_MS / GRID_MS;

/// A market's sampled index changes over the trailing hour.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkoutSpread {
    /// The grid point last sampled, and the index the market had there; `None` when it had none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<(i64, Fixed)>,
    /// Each counted change's square, in bps², by the grid point that ends its interval, oldest first.
    #[serde(default, skip_serializing_if = "VecDeque::is_empty")]
    pub squares: VecDeque<(i64, Fixed)>,
}

impl MarkoutSpread {
    /// The root mean square of the changes counted over the trailing hour of the grid points before
    /// `end`, rounded down, or `None` with none counted.
    fn rms_bps(&self, end: i64) -> Option<Fixed> {
        let (sum, count) = self
            .squares
            .iter()
            .filter(|(at, _)| *at >= end - WINDOW)
            .fold((0i128, 0i128), |(sum, count), (_, square)| (sum + square.raw(), count + 1));
        // At most `WINDOW` squares of at most `MAX_MARKOUT_BPS`² each: the scaled mean fits a u128.
        let scaled = u128::try_from(sum / count.max(1)).expect("squares are positive") * Fixed::ONE.raw() as u128;
        (count > 0).then(|| Fixed::from_raw(scaled.isqrt() as i128))
    }
}

/// The first grid point at or after `ts`.
fn grid_at_or_after(ts: Millis) -> i64 {
    ts.div_euclid(GRID_MS) + i64::from(ts.rem_euclid(GRID_MS) != 0)
}

/// The moments from `from` up to but not including `to` at which one of the market's quotes ages out
/// and leaves it with no index. Every stored quote is stamped no later than the command that stored
/// it, so between commands only these moments change what the quotes give.
fn lapses(market: &MarketState, from: Millis, to: Millis) -> Vec<Millis> {
    let max_age_ms = market.spec.price_rules.max_age_ms;
    market
        .quotes
        .values()
        .map(|quote| quote.ts.saturating_add(max_age_ms).saturating_add(1))
        .filter(|at| (from..to).contains(at) && market.index(*at).is_err())
        .collect()
}

/// The square of the change from `before` to `after`, in bps of `before`, clamped as a markout is.
fn change_square(before: Fixed, after: Fixed) -> Fixed {
    let max = Fixed::from_int(MAX_MARKOUT_BPS);
    // Both are valid indexes, so the change in bps is far inside `Fixed`'s range.
    let bps = (after - before).mul_div(Fixed::from_int(10_000), before, Rounding::TowardZero).clamp(-max, max);
    bps.mul(bps)
}

impl Engine {
    /// From `V54`, samples every market's index at the grid points from `last_now`, the previous
    /// command's time, up to but not including the sequenced time, as the commands before this one
    /// left the market (see the module docs). After a gap longer than the trailing hour, only its
    /// last hour is sampled. From `V58` the moments its quotes aged out since `last_now` are checked
    /// too, also when no grid point has passed.
    pub(crate) fn sample_markout_spreads(&mut self, last_now: Millis) {
        if self.protocol_version < ProtocolVersion::V54 {
            return;
        }
        let end = grid_at_or_after(self.now);
        let first = grid_at_or_after(last_now).max(end - WINDOW - 1);
        if first >= end && self.protocol_version < ProtocolVersion::V58 {
            return;
        }
        // With no grid point passed, only a lapse can change a market, and only one with an open interval.
        let ids: Vec<MarketId> = if first < end {
            self.markets.keys().copied().collect()
        } else {
            self.markout_spreads.iter().filter(|(_, spread)| spread.last.is_some()).map(|(id, _)| *id).collect()
        };
        for id in ids {
            self.sample_market(id, last_now, first, end);
        }
    }

    /// Samples one market at the grid points from `first` up to `end`, the previous command having
    /// been at `last_now`, and drops the changes the trailing hour before `end` no longer holds. From
    /// `V58` an interval in which a quote aged out and left the market no index since `last_now`
    /// counts no change, and ends if it is still open. The market's history is taken for change only
    /// when the sampling changes it.
    fn sample_market(&mut self, id: MarketId, last_now: Millis, first: i64, end: i64) {
        let market = &self.markets[&id];
        let schedule = &market.spec.schedule;
        let kept = self.markout_spreads.get(&id);
        let mut last = kept.and_then(|spread| spread.last);
        let lapsed_at =
            if self.protocol_version >= ProtocolVersion::V58 { lapses(market, last_now, self.now) } else { vec![] };
        let lapsed = |from: Millis, to: Millis| lapsed_at.iter().any(|at| from < *at && *at < to);
        let mut added = Vec::new();
        for at in first..end {
            let ts = at * GRID_MS;
            let tradeable = schedule.is_open(ts) && !market.corporate_action_blocked;
            // A fresh index's quotes are at most their age old, which bounds the span checked.
            let index = tradeable
                .then(|| market.index(ts).ok())
                .flatten()
                .filter(|_| schedule.open_throughout(last_now, ts))
                .map(|index| index.mid);
            if let (Some(mid), Some((previous, before))) = (index, last)
                && previous == at - 1
                && schedule.open_throughout(ts - GRID_MS, ts)
                && !lapsed(ts - GRID_MS, ts)
            {
                added.push((at, change_square(before, mid)));
            }
            last = index.map(|mid| (at, mid));
        }
        // A lapse after the last grid point sampled ends the interval it opened.
        if last.is_some_and(|(at, _)| lapsed(at * GRID_MS, self.now)) {
            last = None;
        }
        let stale = kept.is_some_and(|spread| spread.squares.front().is_some_and(|(at, _)| *at < end - WINDOW));
        if added.is_empty() && !stale && kept.and_then(|spread| spread.last) == last {
            return;
        }
        let spread = self.markout_spreads.entry(id).or_default();
        spread.last = last;
        spread.squares.extend(added);
        while spread.squares.front().is_some_and(|(at, _)| *at < end - WINDOW) {
            spread.squares.pop_front();
        }
    }

    /// Stores a price command's quotes as `store_quotes` does, and from `V54` ends the market's open
    /// grid interval, so it counts no change, when the market has no tradeable index just before the
    /// quotes are stored or just after.
    pub(crate) fn store_sampled_quotes(
        &mut self,
        market: MarketId,
        quotes: &[SourceQuote],
        events: &mut Vec<Event>,
    ) -> bool {
        self.end_unpriced_interval(market);
        let stored = self.store_quotes(market, quotes, events);
        if stored {
            self.end_unpriced_interval(market);
        }
        stored
    }

    /// From `V54`, forgets the grid point the market's next change would count from when the market
    /// has no tradeable index now: just before and just after a price command or a relisting of it.
    pub(crate) fn end_unpriced_interval(&mut self, id: MarketId) {
        let open = self.markout_spreads.get(&id).is_some_and(|spread| spread.last.is_some());
        let Some(market) = self.markets.get(&id).filter(|_| open && self.protocol_version >= ProtocolVersion::V54)
        else {
            return;
        };
        let priced = market.spec.schedule.is_open(self.now)
            && !market.corporate_action_blocked
            && market.index(self.now).is_ok();
        if !priced {
            self.markout_spreads.get_mut(&id).expect("open above").last = None;
        }
    }

    /// The market's markout spread in bps at the sequenced time: the root mean square of the changes
    /// its trailing hour counts, or its floor when that is higher or none are counted. `None` for an
    /// unknown market or one whose listing names no floor, as none does from `V54`.
    pub fn markout_spread_bps(&self, market: MarketId) -> Option<Fixed> {
        let floor = self.markets.get(&market)?.spec.markout_floor_bps?;
        let rms = self.markout_spreads.get(&market).and_then(|spread| spread.rms_bps(grid_at_or_after(self.now)));
        Some(rms.map_or(floor, |rms| rms.max(floor)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(s: &str) -> Fixed {
        s.parse().unwrap()
    }

    #[test]
    fn the_grid_point_at_or_after_a_time() {
        assert_eq!(grid_at_or_after(0), 0);
        assert_eq!(grid_at_or_after(1), 1);
        assert_eq!(grid_at_or_after(GRID_MS), 1);
        assert_eq!(grid_at_or_after(GRID_MS + 1), 2);
    }

    #[test]
    fn a_change_squares_in_bps_of_the_earlier_index_and_clamps() {
        assert_eq!(change_square(f("100"), f("100.01")), f("1"));
        assert_eq!(change_square(f("100"), f("99.98")), f("4"));
        assert_eq!(change_square(f("1"), f("1000")), f("1000000"));
    }

    #[test]
    fn the_root_mean_square_reads_only_the_trailing_hour() {
        let spread =
            MarkoutSpread { last: None, squares: [(10, f("100")), (WINDOW + 8, f("1")), (WINDOW + 9, f("9"))].into() };
        // The hour before grid point WINDOW + 10 holds all three: sqrt(110 / 3), rounded down.
        assert_eq!(spread.rms_bps(WINDOW + 10), Some(f("6.055300708")));
        // A grid point on, the oldest has left it: sqrt((1 + 9) / 2).
        assert_eq!(spread.rms_bps(WINDOW + 11), Some(f("2.236067977")));
        assert_eq!(spread.rms_bps(3 * WINDOW), None);
        assert_eq!(MarkoutSpread::default().rms_bps(0), None);
    }
}
