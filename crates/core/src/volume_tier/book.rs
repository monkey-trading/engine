//! An account's volume book: its counted volume by UTC day, and the notional waiting to count. The
//! book counts lazily: a lot counts on its day once its time has come, whenever the book is next
//! read or changed, so a read at any time sees what has counted by then.
//!
//! A closing lot keeps the fill's closed leg as the toxicity score keeps it (`toxicity::marking`): the
//! quantity the fill closed, less what it matched as a round trip, which a later fill opening on the
//! other side within the horizon takes newest first, whatever its index. Taken at the closing fill's
//! index it is a round trip: of the leg's units, the matured ones are taken first, and those count
//! nothing and are matured again. Taken at another index, the leg's pending units go first, and what
//! the close counted stands. Merging lots never raises the notional a unit of them counts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::Engine;
use crate::execution::Exec;
use crate::fixed::Rounding;
use crate::toxicity::{FillMark, HORIZON_MS};
use crate::types::{AccountId, MarketId, Millis, Side};
use crate::volume_tier::{MIN_HOLD_MS, WINDOW_DAYS, day_of};

/// The most opening lots one position keeps; past it a new lot joins the newest, which then matures
/// with it, later than it would have.
pub const MAX_OPENING_LOTS: usize = 16;
/// The most closing lots one account keeps; past it the oldest is dropped and counts nothing.
pub const MAX_CLOSING_LOTS: usize = 256;

/// An account's counted volume and the notional waiting to count, committed in its own state-tree
/// leaf (kind 28).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountVolume {
    /// Counted volume by UTC day, ascending; a day before the window is dropped when the book next
    /// changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<DayVolume>,
    /// Each position's opening notional waiting out `MIN_HOLD_MS`, by market, oldest first.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub opening: BTreeMap<MarketId, Vec<OpeningLot>>,
    /// Matured notional closed within the round-trip horizon, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closing: Vec<ClosingLot>,
}

/// The volume counted on one UTC day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DayVolume {
    /// Days since the epoch.
    pub day: i64,
    pub volume: Fixed,
}

/// Opening quantity a position still holds, and its USD notional at the fill's price, which counts on
/// the day it matures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpeningLot {
    pub matures_at: Millis,
    pub qty: Fixed,
    pub notional: Fixed,
}

/// A fill's closed leg within the round-trip horizon, and the matured quantity it closed with its USD
/// notional at the fill's price, which counts on the fill's day once the horizon has passed, unless a
/// reopening at `index` takes it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosingLot {
    pub market: MarketId,
    /// The closing fill's side.
    pub side: Side,
    /// The index the closing fill priced from.
    pub index: Fixed,
    pub closed_at: Millis,
    /// The quantity a reopening may still take, as the toxicity score's closed leg.
    pub leg: Fixed,
    /// Of `leg`, the matured units, which a round trip takes back; the rest closed pending quantity.
    pub matured_leg: Fixed,
    /// The matured quantity closed, which counts.
    pub qty: Fixed,
    pub notional: Fixed,
}

impl ClosingLot {
    /// When the lot counts: the round-trip horizon after the close, when no reopening can match it.
    fn counts_at(&self) -> Millis {
        self.closed_at + HORIZON_MS
    }
}

impl AccountVolume {
    /// Whether the book holds nothing, so its leaf is removed.
    fn is_empty(&self) -> bool {
        self.days.is_empty() && self.opening.is_empty() && self.closing.is_empty()
    }

    /// Counts every lot whose time has come by `now` on its day, and drops the days before the window
    /// that ends on `now`'s day.
    fn settle(&mut self, now: Millis) {
        for lots in self.opening.values_mut() {
            let matured = lots.partition_point(|lot| lot.matures_at <= now);
            for lot in lots.drain(..matured) {
                count(&mut self.days, day_of(lot.matures_at), lot.notional);
            }
        }
        self.opening.retain(|_, lots| !lots.is_empty());
        let due = self.closing.partition_point(|lot| lot.counts_at() <= now);
        for lot in self.closing.drain(..due) {
            count(&mut self.days, day_of(lot.closed_at), lot.notional);
        }
        let first = day_of(now) - (WINDOW_DAYS as i64 - 1);
        self.days.retain(|day| day.day >= first);
    }

    /// The volume counted on each day of the window that ends on `now`'s day, oldest first, every lot
    /// whose time has come by `now` included.
    pub(crate) fn window(&self, now: Millis) -> [Fixed; WINDOW_DAYS] {
        let first = day_of(now) - (WINDOW_DAYS as i64 - 1);
        let mut window = [Fixed::ZERO; WINDOW_DAYS];
        let mut add = |day: i64, notional: Fixed| {
            if let Some(slot) = usize::try_from(day - first).ok().and_then(|at| window.get_mut(at)) {
                *slot = plus(*slot, notional);
            }
        };
        self.days.iter().for_each(|day| add(day.day, day.volume));
        let opening = self.opening.values().flatten().filter(|lot| lot.matures_at <= now);
        opening.for_each(|lot| add(day_of(lot.matures_at), lot.notional));
        let closing = self.closing.iter().filter(|lot| lot.counts_at() <= now);
        closing.for_each(|lot| add(day_of(lot.closed_at), lot.notional));
        window
    }

    /// The notional whose time to count has not come by `now`.
    pub(crate) fn pending(&self, now: Millis) -> Fixed {
        let opening = self.opening.values().flatten().filter(|lot| lot.matures_at > now).map(|lot| lot.notional);
        let closing = self.closing.iter().filter(|lot| lot.counts_at() > now).map(|lot| lot.notional);
        opening.chain(closing).fold(Fixed::ZERO, plus)
    }

    /// Takes up to `qty` from `market`'s opening lots, newest first, each lot's notional falling with
    /// its quantity, rounded down; returns the quantity taken.
    fn consume_opening(&mut self, market: MarketId, qty: Fixed) -> Fixed {
        let Some(lots) = self.opening.get_mut(&market) else { return Fixed::ZERO };
        let mut left = qty;
        while left.is_positive() {
            let Some(lot) = lots.last_mut() else { break };
            if lot.qty <= left {
                left -= lot.qty;
                lots.pop();
            } else {
                lot.notional = lot.notional.mul_div(lot.qty - left, lot.qty, Rounding::Down);
                lot.qty -= left;
                left = Fixed::ZERO;
            }
        }
        if lots.is_empty() {
            self.opening.remove(&market);
        }
        qty - left
    }

    /// Adds an opening lot to `market`'s, joining the newest when it matures at the same time or the
    /// position already keeps `MAX_OPENING_LOTS`, at the lower of their notionals a unit (`merged`).
    fn push_opening(&mut self, market: MarketId, lot: OpeningLot) {
        let lots = self.opening.entry(market).or_default();
        let full = lots.len() >= MAX_OPENING_LOTS;
        match lots.last_mut() {
            Some(last) if full || last.matures_at == lot.matures_at => {
                let qty = last.qty + lot.qty;
                let notional = merged(qty, (last.qty, last.notional), (lot.qty, lot.notional));
                *last = OpeningLot { matures_at: lot.matures_at, qty, notional };
            }
            _ => lots.push(lot),
        }
    }

    /// Adds a closing lot, joining the newest when it closed in the same market, on the same side, at
    /// the same index and time, at the lower of their notionals a unit; past `MAX_CLOSING_LOTS` the
    /// oldest is dropped and counts nothing.
    fn push_closing(&mut self, lot: ClosingLot) {
        let key = |lot: &ClosingLot| (lot.market, lot.side, lot.index, lot.closed_at);
        match self.closing.last_mut() {
            Some(last) if key(last) == key(&lot) => {
                let qty = last.qty + lot.qty;
                last.notional = merged(qty, (last.qty, last.notional), (lot.qty, lot.notional));
                (last.leg, last.matured_leg, last.qty) = (last.leg + lot.leg, last.matured_leg + lot.matured_leg, qty);
            }
            _ => {
                if self.closing.len() >= MAX_CLOSING_LOTS {
                    self.closing.remove(0);
                }
                self.closing.push(lot);
            }
        }
    }

    /// Takes `qty` that a fill opens at `index` from the closed legs of the lots of fills on `side` in
    /// `market`, newest first, as the toxicity score matches it; returns the matured quantity it takes
    /// back. From a lot at `index`, a round trip, it takes the lot's matured units first, which then
    /// count nothing, their notional falling with them, rounded down; from a lot at another index, the
    /// leg's pending units first, so what the lot counts stands.
    fn reopen(&mut self, market: MarketId, side: Side, index: Fixed, qty: Fixed) -> Fixed {
        let (mut left, mut restored) = (qty, Fixed::ZERO);
        for lot in self.closing.iter_mut().rev().filter(|lot| (lot.market, lot.side) == (market, side)) {
            if !left.is_positive() {
                break;
            }
            let taken = lot.leg.min(left);
            if lot.index == index {
                let back = taken.min(lot.matured_leg);
                if back.is_positive() {
                    lot.notional = lot.notional.mul_div(lot.qty - back, lot.qty, Rounding::Down);
                    lot.qty -= back;
                    lot.matured_leg -= back;
                    restored += back;
                }
            } else {
                // The leg's pending units go first; past them the matured ones, which still count.
                let pending = lot.leg - lot.matured_leg;
                lot.matured_leg -= taken - taken.min(pending);
            }
            lot.leg -= taken;
            left -= taken;
        }
        self.closing.retain(|lot| lot.leg.is_positive() || lot.qty.is_positive());
        restored
    }
}

impl Engine {
    /// From `V59`, counts a booked fill in its account's volume book (see `volume_tier`). The book
    /// first counts every lot whose time has come. The quantity the fill closes takes the position's
    /// opening lots, newest first, and counts nothing for them; what it closes past them is matured,
    /// and its USD notional at the fill's price counts on today's day at once for a close-out and
    /// otherwise waits the round-trip horizon in a closing lot, whose leg is the quantity closed less
    /// `closed_matched`, what the toxicity score matched of it as a round trip. The quantity the fill
    /// opens takes the account's closed legs on the other side (`AccountVolume::reopen`): the matured
    /// quantity a round trip takes back is matured again, and the rest is a new opening lot, maturing
    /// `MIN_HOLD_MS` from now.
    pub(crate) fn count_volume(&mut self, exec: &Exec, mark: &FillMark, closed_matched: Fixed) {
        if self.volume_tiers_in_force().is_none() {
            return;
        }
        let now = self.hold_clock_time();
        let currency = self.markets[&exec.market].spec.quote_currency;
        let usd = |qty: Fixed| currency.to_usd(qty.mul(mark.price), mark.fx, Rounding::Down);
        let closes = !mark.held.is_zero() && mark.held.signum() != exec.side.sign();
        let closing = if closes { mark.qty.min(mark.held.abs()) } else { Fixed::ZERO };
        let opening = mark.qty - closing;
        let volume = self.volumes.entry(exec.account).or_default();
        volume.settle(now);
        let matured = closing - volume.consume_opening(exec.market, closing);
        let leg = closing - closed_matched;
        if exec.liquidation {
            // A close-out is no round trip and opens nothing.
            count(&mut volume.days, day_of(now), usd(matured));
        } else if leg.is_positive() || matured.is_positive() {
            let (market, side, index, notional) = (exec.market, exec.side, mark.index, usd(matured));
            // What the close matched as a round trip was opened within the horizon, so pending: every
            // matured unit is in the leg.
            let matured_leg = matured.min(leg);
            let lot = ClosingLot { market, side, index, closed_at: now, leg, matured_leg, qty: matured, notional };
            volume.push_closing(lot);
        }
        let restored = if opening.is_positive() {
            volume.reopen(exec.market, exec.side.opposite(), mark.index, opening)
        } else {
            Fixed::ZERO
        };
        let fresh = opening - restored;
        if fresh.is_positive() {
            let lot = OpeningLot { matures_at: now + MIN_HOLD_MS, qty: fresh, notional: usd(fresh) };
            volume.push_opening(exec.market, lot);
        }
        if volume.is_empty() {
            self.volumes.remove(&exec.account);
        }
    }

    /// At `V59`'s activation, gives each held position an opening lot of the whole position that matures
    /// `MIN_HOLD_MS` later and counts nothing: no fill before the activation counted, and a close in the
    /// first minute takes it as pending, since no record says which units were held that long.
    pub(super) fn hold_held_positions(&mut self) {
        let matures_at = self.hold_clock_time() + MIN_HOLD_MS;
        self.ensure_hot_indexes();
        let holders = &self.hot_indexes.as_ref().expect("initialized").holders;
        let held: Vec<(MarketId, AccountId)> =
            holders.iter().flat_map(|(market, accounts)| accounts.iter().map(|account| (*market, *account))).collect();
        for (market, account) in held {
            let qty = self.accounts[&account].position(market).qty.abs();
            let lot = OpeningLot { matures_at, qty, notional: Fixed::ZERO };
            self.volumes.entry(account).or_default().push_opening(market, lot);
        }
    }

    /// Scales `account`'s opening lots in `market` by a split of `old` units into `new`, each rounded
    /// toward zero as the position is, so they never exceed it; their notional keeps its value. The book
    /// first counts every lot whose time has come, so a matured lot rounded to nothing still counts.
    pub(crate) fn scale_volume_lots(&mut self, account: AccountId, market: MarketId, new: u32, old: u32) {
        if !self.volumes.get(&account).is_some_and(|volume| volume.opening.contains_key(&market)) {
            return;
        }
        let now = self.hold_clock_time();
        let volume = self.volumes.get_mut(&account).expect("checked above");
        volume.settle(now);
        let Some(lots) = volume.opening.get_mut(&market) else { return };
        // A lot is at most its position, whose scaled quantity the split checked is in range.
        lots.iter_mut().for_each(|lot| lot.qty = Fixed::from_raw(lot.qty.raw() * i128::from(new) / i128::from(old)));
        lots.retain(|lot| lot.qty.is_positive());
        if lots.is_empty() {
            volume.opening.remove(&market);
        }
        if volume.is_empty() {
            self.volumes.remove(&account);
        }
    }
}

/// Adds `notional` to `day`'s volume in `days`, kept ascending.
fn count(days: &mut Vec<DayVolume>, day: i64, notional: Fixed) {
    // A lot that counts nothing, such as a closing lot kept only for its leg, adds no day.
    if notional.is_zero() {
        return;
    }
    match days.binary_search_by_key(&day, |entry| entry.day) {
        Ok(at) => days[at].volume = plus(days[at].volume, notional),
        Err(at) => days.insert(at, DayVolume { day, volume: notional }),
    }
}

/// The notional of `qty` merged from lots of `(quantity, notional)` `a` and `b`, at the lower of their
/// notionals a unit, rounded down, so any part of the merged lot counts no more than the same part of
/// either would have.
fn merged(qty: Fixed, a: (Fixed, Fixed), b: (Fixed, Fixed)) -> Fixed {
    // A lot with no quantity, a closing lot whose leg closed nothing matured, has no notional to weigh.
    [a, b]
        .into_iter()
        .filter(|(lot_qty, _)| lot_qty.is_positive())
        .map(|(lot_qty, notional)| notional.mul_div(qty, lot_qty, Rounding::Down))
        .min()
        .unwrap_or(Fixed::ZERO)
}

/// `a + b`, saturating, so no number of fills can overflow a day's volume.
fn plus(a: Fixed, b: Fixed) -> Fixed {
    Fixed::from_raw(a.raw().saturating_add(b.raw()))
}

#[cfg(test)]
mod tests;
