//! The house's marked loss to its clients over the trailing day, and the stress trigger on it, from
//! `V59`.
//!
//! The loss is the fall in the house's trading result over the window. The result is house NAV,
//! which counts clients' unrealised PnL at the marks, less the house capital credited in net since
//! `V59`: capital moves and deposits into the house, less distributions out of it. So the loss is
//! net of fees, spreads and financing, and counts a winning position before it closes: a leg held
//! open across an unpriced move counts once its market prices again, whatever the other leg's
//! close-out realises. A cash dividend paid at its ex-date open while its market's mark still
//! includes it counts that market without it until the market takes a fresh mark, so the payment is
//! not read as a loss before the price falls by it; a split in between leaves the value unchanged.
//!
//! The window opens at `V59`'s activation and keeps, for each `SLOT_MS` slot a command ran in, the
//! result after its last command and the highest after any. The result only moves with a command,
//! so the result at the end of a slot holds until the next command. The loss is measured from the
//! window's start, a day ago: where a command ran in the slot holding it, from the highest result in
//! that slot or at the end of the slot before, so the loss read is never below the loss over the
//! trailing day; otherwise from the result at the end of the newest slot before it. The window keeps
//! at most 98 slots however often commands arrive.
//!
//! A stress level's trigger may name a loss threshold (`StressTrigger::house_loss`), met when the
//! loss is above it. It applies in every market: the stress readings take the loss before every fill
//! and after every command (`stress`), and a market's level steps down from it after the cooldown,
//! as from any other trigger. Its bounds are in `bounds`.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::Engine;
use crate::event::Event;
use crate::fixed::Rounding;
use crate::params::StressLevel;
use crate::protocol::ProtocolVersion;
use crate::types::{MarketId, Millis};

mod bounds;
pub use bounds::{ACTIVATION_LEVEL, LossTrigger, loss_triggers};

/// The trailing window the loss is measured over: a day.
pub const WINDOW_MS: Millis = 24 * 60 * 60 * 1000;
/// Each slot the window keeps one result for: 15 minutes, so the window reaches back at most about
/// 1% more than a day.
pub const SLOT_MS: Millis = 15 * 60 * 1000;

/// What the window keeps, left out of the encoding while empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HouseLoss {
    /// House capital credited in net since `V59`, which the trading result leaves out of NAV.
    #[serde(default, skip_serializing_if = "Fixed::is_zero")]
    pub capital: Fixed,
    /// The trading result in each slot that had a command, oldest first, from the newest slot that
    /// ended before the window's start.
    #[serde(default, skip_serializing_if = "VecDeque::is_empty")]
    pub slots: VecDeque<SlotResult>,
    /// The stress level the loss set after the last command: the highest whose threshold it was above.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub level: usize,
    /// Each market whose mark still includes a cash dividend paid at its ex-date open, with what that
    /// dividend is worth to its clients before withholding: their entitled quantity times the dividend
    /// per unit, in the market's currency, rounded down. It holds until the market takes a fresh mark.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unpriced_dividends: BTreeMap<MarketId, Fixed>,
}

/// The trading result in a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotResult {
    /// The slot's start over `SLOT_MS`.
    pub slot: i64,
    /// After the slot's last command.
    pub result: Fixed,
    /// The highest after any of the slot's commands.
    pub high: Fixed,
}

/// The loss as the last command left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct HouseLossReading {
    /// In USD; positive when the house lost money to its clients over the window.
    pub loss: Fixed,
    /// The start of the slot the window is measured from.
    pub since: Millis,
    /// The stress level it sets: the highest whose threshold it is above, 0 when none.
    pub level: usize,
}

impl HouseLoss {
    /// Whether nothing is recorded, so earlier state encodings stand.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The result the loss is measured from at `now`, and the start of the slot that holds it: the
    /// end of the newest slot before the window's start or, where a command ran in the slot holding
    /// the start, the highest of that and the slot's results, which bounds the result at the start
    /// from above. While the window reaches back further than the activation, the activation's.
    fn start(&self, now: Millis) -> Option<(Fixed, Millis)> {
        let first = window_start_slot(now);
        let before = self.slots.partition_point(|slot| slot.slot < first);
        let anchor = self.slots.get(before.saturating_sub(1))?;
        let holding = self.slots.get(before).filter(|slot| slot.slot == first && before > 0);
        let result = holding.map_or(anchor.result, |slot| slot.high.max(anchor.result));
        Some((result, (anchor.slot + 1) * SLOT_MS))
    }
}

impl Engine {
    /// The loss at house NAV `nav`: the trading result at the window's start less the result at
    /// `nav`. Zero before `V59`.
    pub(crate) fn house_loss_at(&self, nav: Fixed) -> Fixed {
        match self.house_loss.start(self.now) {
            Some((start, _)) => start - self.trading_result(nav),
            None => Fixed::ZERO,
        }
    }

    /// The trading result at house NAV `nav`: NAV with each market whose mark still includes a cash
    /// dividend paid valued without it, in USD rounded down, less the capital credited in net.
    fn trading_result(&self, nav: Fixed) -> Fixed {
        let unpriced_dividends: Fixed = self
            .house_loss
            .unpriced_dividends
            .iter()
            .filter_map(|(id, value)| Some(self.markets.get(id)?.usd_at_mark(*value, Rounding::Down)))
            .sum();
        nav + unpriced_dividends - self.house_loss.capital
    }

    /// Opens the window at the trading result now, as both the slot before the current one and the
    /// current one, so a change in the next command counts. `V59`'s activation runs it.
    pub(crate) fn open_window(&mut self) {
        let result = self.trading_result(self.house_nav());
        let slot = self.now.div_euclid(SLOT_MS);
        let flat = |slot| SlotResult { slot, result, high: result };
        self.house_loss.slots = VecDeque::from([flat(slot - 1), flat(slot)]);
    }

    /// From `V59`, records the trading result at house NAV `nav` in the current slot, drops the slots
    /// before the window's start, and emits `HouseLossChanged` when the level the loss sets changed.
    /// Runs after every command, at the NAV the stress readings take.
    pub(crate) fn record_house_loss(&mut self, nav: Fixed, events: &mut Vec<Event>) {
        if self.protocol_version < ProtocolVersion::V59 {
            return;
        }
        let slot = self.now.div_euclid(SLOT_MS);
        let result = self.trading_result(nav);
        let slots = &mut self.house_loss.slots;
        match slots.back_mut() {
            Some(last) if last.slot == slot => (last.result, last.high) = (result, last.high.max(result)),
            _ => slots.push_back(SlotResult { slot, result, high: result }),
        }
        let start = window_start_slot(self.now);
        while slots.get(1).is_some_and(|next| next.slot < start) {
            slots.pop_front();
        }
        let loss = self.house_loss_at(nav);
        let level = self.params.as_ref().map_or(0, |params| loss_level(&params.stress_levels, loss));
        if level != self.house_loss.level {
            self.house_loss.level = level;
            events.push(Event::HouseLossChanged { level, loss });
        }
    }

    /// From `V59`, notes house capital credited, or with a negative `amount` taken out, which the
    /// trading result leaves out.
    pub(crate) fn note_house_capital(&mut self, amount: Fixed) {
        if self.protocol_version >= ProtocolVersion::V59 {
            self.house_loss.capital += amount;
        }
    }

    /// From `V59`, notes a cash dividend paid at `market`'s ex-date open, worth `value` to its clients
    /// before withholding in the market's currency, which the market's mark includes until it takes a
    /// fresh one.
    pub(crate) fn note_unpriced_dividend(&mut self, market: MarketId, value: Fixed) {
        if self.protocol_version >= ProtocolVersion::V59 {
            *self.house_loss.unpriced_dividends.entry(market).or_default() += value;
        }
    }

    /// Notes that `market` took a fresh mark, which has the dividends paid since its last one.
    pub(crate) fn note_fresh_mark(&mut self, market: MarketId) {
        self.house_loss.unpriced_dividends.remove(&market);
    }

    /// From `V59`, the stress level the house's loss set after the last command.
    pub fn house_loss_level(&self) -> usize {
        self.house_loss.level
    }

    /// From `V59`, the loss as the last command left it, at the current marks.
    pub fn house_loss(&self) -> Option<HouseLossReading> {
        let (_, since) = self.house_loss.start(self.now)?;
        Some(HouseLossReading { loss: self.house_loss_at(self.house_nav()), since, level: self.house_loss.level })
    }
}

/// The highest stress level in `levels` whose loss threshold `loss` is above, or 0.
pub(crate) fn loss_level(levels: &[StressLevel], loss: Fixed) -> usize {
    levels
        .iter()
        .rposition(|level| level.trigger.as_ref().and_then(|t| t.house_loss).is_some_and(|threshold| loss > threshold))
        .unwrap_or(0)
}

/// The slot holding the window's start at `now`.
fn window_start_slot(now: Millis) -> i64 {
    (now - WINDOW_MS).div_euclid(SLOT_MS)
}

/// Whether `level` is 0, which the encoding leaves out.
fn is_zero(level: &usize) -> bool {
    *level == 0
}

#[cfg(test)]
mod tests;
