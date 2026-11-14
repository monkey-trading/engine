//! The market leaf: a market's mark, quote currency, FX mark and position totals, with the hash of
//! the rest of its state.

use std::collections::BTreeMap;

use alloy_primitives::B256;
use alloy_sol_types::SolValue;
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::{MarketState, StressState};
use crate::futures::FuturesState;
use crate::market::{MarketSpec, QuoteCurrency};
use crate::merkle::keccak;
use crate::merkle::leaf::{json_hash, typed};
use crate::price::{Index, SourceQuote};
use crate::types::Millis;
use crate::weekend::WeekendGap;

/// A market as its leaf holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketLeaf {
    /// The mark positions are valued at, once the market has been priced.
    pub mark: Option<Fixed>,
    /// The currency the market is quoted in, and the market whose index converts it to USD.
    pub quote_currency: QuoteCurrency,
    /// The index amounts convert to USD at, once that market has priced it.
    pub fx_mark: Option<Fixed>,
    pub client_qty: Fixed,
    pub client_cost: Fixed,
    /// keccak-256 of the JSON encoding of the market's spec, venue quotes, open and reduce-only
    /// flags, stress state, from `V12` the index it was last processed at, from `V21` any roll
    /// state, and from `V59` any weekend gap.
    pub state_hash: B256,
}

/// What a market's `state_hash` covers.
#[derive(Serialize)]
struct MarketRest<'a> {
    spec: &'a MarketSpec,
    quotes: &'a BTreeMap<String, SourceQuote>,
    open: bool,
    reduce_only: bool,
    stress: &'a StressState,
    /// Left out while `None`, as it is before `V12`, so earlier roots stand.
    #[serde(skip_serializing_if = "Option::is_none")]
    processed_index: &'a Option<Index>,
    /// Left out while `None`, as it is for every market before `V21`, so earlier roots stand.
    #[serde(skip_serializing_if = "Option::is_none")]
    futures: &'a Option<FuturesState>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    corporate_action_blocked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_split_at: &'a Option<Millis>,
    /// Left out while `None`, as it is for every market before `V59`, so earlier roots stand.
    #[serde(skip_serializing_if = "Option::is_none")]
    weekend: &'a Option<WeekendGap>,
}

impl MarketLeaf {
    /// The leaf of `market`.
    pub(crate) fn of(market: &MarketState) -> Self {
        // `resting` indexes orders the order leaves hold.
        let MarketState {
            spec,
            quotes,
            open,
            reduce_only,
            mark,
            fx_mark,
            client_qty,
            client_cost,
            stress,
            resting: _,
            processed_index,
            futures,
            corporate_action_blocked,
            last_split_at,
            weekend,
        } = market;
        let rest = MarketRest {
            spec,
            quotes,
            open: *open,
            reduce_only: *reduce_only,
            stress,
            processed_index,
            futures,
            corporate_action_blocked: *corporate_action_blocked,
            last_split_at,
            weekend,
        };
        MarketLeaf {
            mark: *mark,
            quote_currency: spec.quote_currency,
            fx_mark: *fx_mark,
            client_qty: *client_qty,
            client_cost: *client_cost,
            state_hash: json_hash(&rest),
        }
    }

    /// The leaf's value hash.
    pub fn value_hash(&self) -> B256 {
        let (quote_currency, fx_market) = match self.quote_currency {
            QuoteCurrency::Usd => (0, 0),
            QuoteCurrency::Eur { fx_market } => (1, fx_market),
            QuoteCurrency::Gbp { fx_market } => (2, fx_market),
            QuoteCurrency::Jpy { fx_market } => (3, fx_market),
        };
        let leaf = typed::MarketLeaf {
            marked: self.mark.is_some(),
            mark: self.mark.unwrap_or(Fixed::ZERO).raw(),
            quoteCurrency: quote_currency,
            fxMarket: fx_market,
            fxMarked: self.fx_mark.is_some(),
            fxMark: self.fx_mark.unwrap_or(Fixed::ZERO).raw(),
            clientQty: self.client_qty.raw(),
            clientCost: self.client_cost.raw(),
            stateHash: self.state_hash,
        };
        keccak(&[&leaf.abi_encode()])
    }
}
