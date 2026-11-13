//! A fill and why a resting order was cancelled, as events carry them.

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::types::{AccountId, MarketId, Millis, OrderId, Side};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    Client,
    /// Triggered but couldn't fill (margin, house capacity, or reduce-only
    /// with no position left).
    Unfillable,
    /// A split rounded its quantity or price to zero, or took its quantity past the input bound.
    Split,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub account: AccountId,
    pub order_id: OrderId,
    pub market: MarketId,
    pub side: Side,
    pub qty: Fixed,
    pub price: Fixed,
    /// Reference mid at fill time. `price` minus `index` is the spread paid.
    pub index: Fixed,
    /// The spread paid in whole basis points of `index`, truncated: a spread under a bp, as an FX or
    /// index market's first band charges from `V52`, reads 0, and `price` minus `index` is exact.
    pub spread_bps: i64,
    /// Price PnL realised by this fill, in USD, before the fee.
    pub realised_pnl: Fixed,
    /// The FX index `realised_pnl` converted to USD at; absent for a USD market.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx_index: Option<Fixed>,
    pub profit_fee: Fixed,
    /// From `V55`, the hold of the slice the fill closes, from its position's open time, and the fee
    /// rate a gain on it pays (`holding_fee`); both absent for a fill that closes nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ms: Option<Millis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profit_fee_bps: Option<i64>,
    /// Signed position after the fill.
    pub position: Fixed,
    pub liquidation: bool,
}
