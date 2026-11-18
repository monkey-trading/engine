//! Where a level is indexed: the index mid past which the position's closing price can reach it.
//!
//! A close pays a spread `s(m)` at index mid `m`, so a long closes at `m - s(m)` and a short at
//! `m + s(m)`. A key search reads two bounds of it, `up(m) >= s(m) >= down(m)`, each non-decreasing in
//! `m`, and at most the close-spread ceiling's share of `m` (plus a raw unit for `up`, which may read
//! the next mid). In a USD market both are the spread itself, which every rounding in a close's price
//! keeps non-decreasing; in a market converting at its own index the spread is not, and `up` and `down`
//! bound it; in one converting at another market's index, whose spread moves with that FX index, `up` is
//! the ceiling and `down` the least fill rate, so an FX move changes no key (`Engine::level_keys`).
//!
//! - A short's closing price `m + s(m)` is bounded by non-decreasing functions, so its key is found by
//!   bisection: a stop loss at the first mid `m + up(m)` reaches it, a take profit at the last mid
//!   `m + down(m)` is within it.
//! - A long's closing price `m - s(m)` need not be monotone: a mid a raw unit higher can close lower
//!   where the rounded spread steps by more than a unit. The search finds a mid where the bound crosses
//!   the level, then proves no mid beyond it reaches the level, using that over `[a, b]` the long's price
//!   is at least `a - up(b)` and at most `b - down(a)`; intervals grow with the room the price has gained,
//!   so a few cover the rest of the ceiling's reach, and a mid that may still reach the level restarts
//!   the search from it.
//!
//! The key is exact for a USD market: no mid past it fires, and the key itself does.

use crate::Fixed;
use crate::hot_indexes::reach;
use crate::levels::LevelKind;
use crate::types::Side;

/// The highest index mid, in raw units: index mids are valid inputs.
const MAX: i128 = Fixed::MAX_INPUT.raw();

/// A level's key: the index mid from which its closing price may reach it, and the side of it a mid
/// must be on: at or below it for a long's stop loss or a short's take profit (`below`), and at or above
/// it otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LevelKey {
    pub(crate) mid: Fixed,
    pub(crate) below: bool,
}

/// The bounds of a close's spread a key search reads, in raw units of each mid.
pub(crate) struct Spreads<U: Fn(i128) -> i128, D: Fn(i128) -> i128> {
    pub(crate) up: U,
    pub(crate) down: D,
    /// The close-spread ceiling, in tenths of a bp.
    pub(crate) ceiling: i64,
    /// The lowest mid `up` bounds the spread from; below it a stop loss is keyed at the ceiling's reach.
    pub(crate) floor: i128,
}

impl<U: Fn(i128) -> i128, D: Fn(i128) -> i128> Spreads<U, D> {
    /// The key of a `kind` level at `level` on a long position when `long`, or `None` when no valid mid
    /// can reach it.
    pub(crate) fn key(&self, long: bool, kind: LevelKind, level: Fixed) -> Option<LevelKey> {
        let level = level.raw();
        let (mid, below) = match (long, kind) {
            (true, LevelKind::StopLoss) => (self.long_stop(level), true),
            (true, LevelKind::TakeProfit) => (self.long_take(level)?, false),
            (false, LevelKind::StopLoss) => (self.short_stop(level), false),
            (false, LevelKind::TakeProfit) => (self.short_take(level)?, true),
        };
        Some(LevelKey { mid: Fixed::from_raw(mid), below })
    }

    /// The last mid at which `m - up(m)` is at most `stop`: no mid past the ceiling's reach comes that
    /// low, and every mid at or below `stop` does.
    fn long_stop(&self, stop: i128) -> i128 {
        // Past `end` the ceiling keeps `m - up(m)` above `stop + 1` less the unit `up` may read ahead.
        let end = (sell_reach(stop + 2, self.ceiling) - 1).min(MAX);
        if stop < self.floor {
            return end;
        }
        let price = |m: i128| m - (self.up)(m);
        let mut from = stop.min(MAX);
        loop {
            let last = crossing((from, price(from)), end + 1, stop, price, |value| value <= stop);
            // The last mid known to fire, and the next, known not to.
            let next = last + 1;
            match self.long_stop_dip(stop, next, (self.up)(next), end) {
                None => return last,
                Some(dip) => from = dip,
            }
        }
    }

    /// The first mid in `from..=end` whose long close may come to `stop` or below, given that `from`'s
    /// does not and `up(from)` is `known`, or `None` when none can.
    fn long_stop_dip(&self, stop: i128, from: i128, known: i128, end: i128) -> Option<i128> {
        let (mut a, mut known) = (from + 1, known);
        while a <= end {
            // Over `[a, b]` the close is at least `a - up(b)`, so `up` may still grow by `room - 1`.
            let room = a - stop - known;
            let mut b = (a + growth(room, a - 1, known)).min(end);
            loop {
                let up = (self.up)(b);
                if a - up > stop {
                    (a, known) = (b + 1, up);
                    break;
                }
                if b == a {
                    return Some(a);
                }
                b = a + (b - a) / 2;
            }
        }
        None
    }

    /// The first mid at which `m - down(m)` is at least `take`, or `None` when no valid mid's is: none
    /// below `take`, and the ceiling's reach of it.
    fn long_take(&self, take: i128) -> Option<i128> {
        if take > MAX {
            return None;
        }
        let price = |m: i128| m - (self.down)(m);
        let mut end = sell_reach(take, self.ceiling).min(MAX + 1);
        if end > MAX {
            if price(MAX) < take {
                return self.long_take_dip(take, MAX, (self.down)(MAX)).map(|dip| self.long_take_from(take, dip));
            }
            end = MAX;
        }
        Some(self.long_take_from(take, end))
    }

    /// The first mid at which `m - down(m)` is at least `take`, given that `known` is one.
    fn long_take_from(&self, take: i128, known: i128) -> i128 {
        let price = |m: i128| m - (self.down)(m);
        let mut to = known;
        loop {
            // `take - 1` closes below `take` whatever the spread, and at most at `take - 1`.
            let last = crossing((take - 1, take - 1), to, take, price, |value| value < take);
            match self.long_take_dip(take, last, (self.down)(last)) {
                None => return last + 1,
                Some(dip) => to = dip,
            }
        }
    }

    /// The last mid in `take..to` whose long close may come to `take` or above, given that `to`'s does
    /// not and `down(to)` is `known`, or `None` when none can.
    fn long_take_dip(&self, take: i128, to: i128, known: i128) -> Option<i128> {
        let (mut b, mut known) = (to - 1, known);
        while b >= take {
            // Over `[a, b]` the close is at most `b - down(a)`, so `down` may still fall by `room - 1`.
            let room = take - b + known;
            let mut a = (b - growth(room, b + 1, known)).max(take);
            loop {
                let down = (self.down)(a);
                if b - down < take {
                    (b, known) = (a - 1, down);
                    break;
                }
                if a == b {
                    return Some(b);
                }
                a = b - (b - a) / 2;
            }
        }
        None
    }

    /// The first mid at which `m + up(m)` is at least `stop`: `stop` itself is one, and the ceiling's
    /// reach bounds how low one can be.
    fn short_stop(&self, stop: i128) -> i128 {
        // At or below `lo` the close, at most `m + up(m)`, stays two units under `stop`.
        let lo = buy_reach(stop - 1, self.ceiling) - 1;
        if lo + 1 < self.floor {
            return (lo + 1).max(1);
        }
        let price = |m: i128| m + (self.up)(m);
        let lo = lo.max(0);
        crossing((lo, lo), stop, stop, price, |value| value < stop) + 1
    }

    /// The last mid at which `m + down(m)` is at most `take`, or `None` when no valid mid's is.
    fn short_take(&self, take: i128) -> Option<i128> {
        let price = |m: i128| m + (self.down)(m);
        let lo = buy_reach(take, self.ceiling).max(0);
        let last = crossing((lo, lo), take + 1, take, price, |value| value <= take);
        (last >= 1).then_some(last)
    }
}

/// The last point of a run: from `lo`, on `lo_side`, with `value` about its second item, and `hi`, past
/// `lo` and not on it, the `m` in `lo..hi` on `lo_side` whose next point is not. It interpolates `value`
/// towards `target`, the edge of `lo_side`, and bisects whenever a step does not halve the bracket.
fn crossing(
    lo: (i128, i128),
    hi: i128,
    target: i128,
    value: impl Fn(i128) -> i128,
    lo_side: impl Fn(i128) -> bool,
) -> i128 {
    let ((mut lo, mut at_lo), mut hi, mut at_hi) = (lo, hi, None::<i128>);
    let mut bisect = false;
    while hi - lo > 1 {
        let width = hi - lo;
        let mid = match at_hi {
            Some(at_hi) if !bisect && at_hi != at_lo => {
                // The value moves about as the mid does, so the edge sits about where it reaches `target`.
                (lo + (target - at_lo).saturating_mul(width) / (at_hi - at_lo)).clamp(lo + 1, hi - 1)
            }
            _ => lo + width / 2,
        };
        let at = value(mid);
        if lo_side(at) {
            (lo, at_lo) = (mid, at);
        } else {
            (hi, at_hi) = (mid, Some(at));
        }
        bisect = !bisect && (hi - lo) * 2 > width;
    }
    lo
}

/// How far an interval may reach, `room - 1` raw units of growth at the average rate `spread` over
/// `at` units, with a tenth to spare; everything when the spread is zero.
fn growth(room: i128, at: i128, spread: i128) -> i128 {
    if room <= 1 {
        return 0;
    }
    if spread <= 0 {
        return MAX;
    }
    (room - 1).saturating_mul(at) / spread / 10 * 9
}

/// The lowest mid `m` with `m - floor(m x ceiling / 100,000)` at least `price`, in raw units.
fn sell_reach(price: i128, ceiling: i64) -> i128 {
    reach(Fixed::from_raw(price), Side::Sell, ceiling).raw()
}

/// The highest mid `m` with `m + floor(m x ceiling / 100,000)` at most `price`, in raw units.
fn buy_reach(price: i128, ceiling: i64) -> i128 {
    if price < 0 {
        return -1;
    }
    reach(Fixed::from_raw(price), Side::Buy, ceiling).raw()
}

#[cfg(test)]
mod tests;
