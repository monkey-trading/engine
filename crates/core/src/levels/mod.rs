//! Take-profit and stop-loss levels on a position, from `V79`.
//!
//! - **Set.** An order may name a take profit and a stop loss, each optional, which the position its
//!   fill leaves on the order's side takes, each in place of its own, keeping a level the order does
//!   not name; a resting entry order holds them until it fills. A signed request sets, moves or removes
//!   a position's levels, taking exactly those it names. A reduce-only order naming levels is refused,
//!   as it opens nothing for them to protect.
//! - **Distance.** A level lies at least its market's minimum distance (`MarketSpec`) beyond its
//!   reference price, on its own side, or is refused as `level_too_close`: the closing price of the
//!   whole position now (at the live index, or at the last mark while the market is shut), the
//!   closing price of the position a market order leaves at its fill's index, or a resting entry
//!   order's own price. Only a new or moved level is checked, and only when it is set.
//! - **Trigger.** A level fires when the closing price of the whole position, as a reduce-only close
//!   of it would fill now, reaches it: a long's stop loss at or below it and take profit at or above
//!   it, mirrored for a short. Levels are checked on each price in their market, after its resting
//!   orders and before its close-outs, and when an entry's fill attaches them, never while the market
//!   cannot trade. The stop loss fills at that closing price, gap included; the take profit at its
//!   level or better. A derived index finds the levels a price reaches (`hot_indexes::levels`).
//! - **Lifecycle.** Levels follow the whole position: an add or a partial close keeps them, and every
//!   full close, a reversal and a split that rounds the position to zero removes them, each with an
//!   event saying why.
//! - **Slots.** A market's levels use one slot of the account's resting-order limit; a first level
//!   on a position needs a free one, and a falling tier never removes levels already set.

mod bounds;
mod closing;
mod index;
mod keys;
mod request;
mod trigger;

pub use bounds::check_level_distance_range;
pub(crate) use keys::LevelKey;

use serde::{Deserialize, Serialize};

use crate::Fixed;

/// A position's take profit and stop loss, either absent. Left out of the encoding while absent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PositionLevels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take_profit: Option<Fixed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_loss: Option<Fixed>,
}

/// Which of a position's levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LevelKind {
    TakeProfit,
    StopLoss,
}

/// Why a position's levels were removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LevelsRemoved {
    /// A client's order closed the whole position.
    Closed,
    /// A close-all closed the whole position.
    CloseAll,
    /// The take profit's fill closed the position.
    TakeProfit,
    /// The stop loss's fill closed the position.
    StopLoss,
    /// A close-out for margin closed the position.
    CloseOut,
    /// A weekend close-out closed the position.
    WeekendCloseOut,
    /// A split rounded the position to zero, or scaled a level to zero or past the input bound.
    Split,
    /// A fill turned the position to the other side.
    Reversed,
    /// The fill of the order naming them left no position on the order's side.
    NotOpened,
}

/// What a fill's booking is, which says why a full close removes the position's levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FillCause {
    Order,
    CloseAll,
    Level(LevelKind),
    CloseOut,
    WeekendCloseOut,
}

impl PositionLevels {
    /// The levels `take_profit` and `stop_loss` name, or `None` when they name neither.
    pub fn named(take_profit: Option<Fixed>, stop_loss: Option<Fixed>) -> Option<Self> {
        (take_profit.is_some() || stop_loss.is_some()).then_some(PositionLevels { take_profit, stop_loss })
    }

    /// Whether neither level is set.
    pub fn is_empty(&self) -> bool {
        self.take_profit.is_none() && self.stop_loss.is_none()
    }

    /// These levels set over `held`: each level set here in place of `held`'s, and `held`'s other.
    pub fn over(self, held: PositionLevels) -> PositionLevels {
        PositionLevels {
            take_profit: self.take_profit.or(held.take_profit),
            stop_loss: self.stop_loss.or(held.stop_loss),
        }
    }

    /// The levels set here that `held` does not already hold: those a change sets anew or moves.
    pub fn moved_from(self, held: PositionLevels) -> PositionLevels {
        PositionLevels {
            take_profit: self.take_profit.filter(|level| held.take_profit != Some(*level)),
            stop_loss: self.stop_loss.filter(|level| held.stop_loss != Some(*level)),
        }
    }

    /// The level of `kind`.
    pub fn get(&self, kind: LevelKind) -> Option<Fixed> {
        match kind {
            LevelKind::TakeProfit => self.take_profit,
            LevelKind::StopLoss => self.stop_loss,
        }
    }
}

impl FillCause {
    /// Why a full close by a fill of this cause removes the position's levels.
    pub(crate) fn removal(self) -> LevelsRemoved {
        match self {
            FillCause::Order => LevelsRemoved::Closed,
            FillCause::CloseAll => LevelsRemoved::CloseAll,
            FillCause::Level(LevelKind::TakeProfit) => LevelsRemoved::TakeProfit,
            FillCause::Level(LevelKind::StopLoss) => LevelsRemoved::StopLoss,
            FillCause::CloseOut => LevelsRemoved::CloseOut,
            FillCause::WeekendCloseOut => LevelsRemoved::WeekendCloseOut,
        }
    }
}

#[cfg(test)]
mod tests;
