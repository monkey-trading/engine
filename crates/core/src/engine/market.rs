use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::fixed::Rounding;
use crate::futures::FuturesState;
use crate::market::MarketSpec;
use crate::price::{Index, SourceQuote};
use crate::stress::StressState;
use crate::types::{AccountId, Millis, OrderId};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketState {
    pub spec: MarketSpec,
    pub quotes: BTreeMap<String, SourceQuote>,
    pub open: bool,
    pub reduce_only: bool,
    /// Last index mid marked while the market and its FX market had a price. Positions are marked
    /// here, including while the market is closed or its FX market has no price.
    pub mark: Option<Fixed>,
    /// Last index of the FX market that could convert this market's quote currency: what its
    /// positions are valued at in USD. `None` for a USD market, and left out of the encoding then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fx_mark: Option<Fixed>,
    /// Sum of client position qty; the house holds the opposite.
    pub client_qty: Fixed,
    /// Sum of client `entry * qty`, so unrealised PnL is O(1) per market.
    pub client_cost: Fixed,
    pub stress: StressState,
    /// Resting orders in time priority.
    pub resting: BTreeMap<OrderId, AccountId>,
    /// From `V12`, the index the mark, stress, resting orders, and close-outs were last processed
    /// at, or `None` once the market cannot deal: closed, with no valid index, or with no FX price.
    /// Left out of the encoding while `None`, so roots before `V12` do not change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processed_index: Option<Index>,
    /// From `V21`, where a market priced from futures stands in its roll order. `None` for any other
    /// market, and left out of the encoding then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub futures: Option<FuturesState>,
    /// A failed action prevents dealing on books that need operator intervention.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub corporate_action_blocked: bool,
    /// Quotes stamped before the most recent split are in the previous share units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_split_at: Option<Millis>,
}

impl MarketState {
    /// A newly listed market: closed until its first session check, with no price or positions.
    pub(crate) fn new(spec: MarketSpec) -> Self {
        MarketState {
            spec,
            quotes: BTreeMap::new(),
            open: false,
            reduce_only: false,
            mark: None,
            fx_mark: None,
            client_qty: Fixed::ZERO,
            client_cost: Fixed::ZERO,
            stress: StressState::default(),
            resting: BTreeMap::new(),
            processed_index: None,
            futures: None,
            corporate_action_blocked: false,
            last_split_at: None,
        }
    }

    /// `amount` in this market's quote currency, in USD at its FX mark, rounded as `rounding` says.
    pub fn usd_at_mark(&self, amount: Fixed, rounding: Rounding) -> Fixed {
        self.spec.quote_currency.to_usd(amount, self.fx_mark, rounding)
    }
}
