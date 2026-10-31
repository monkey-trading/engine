use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::Engine;
use crate::event::Event;
use crate::fixed::Rounding;
use crate::futures::FuturesState;
use crate::market::MarketSpec;
use crate::price::{Index, SourceQuote};
use crate::protocol::ProtocolVersion;
use crate::stress::StressState;
use crate::types::{AccountId, MarketId, Millis, OrderId};

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

impl Engine {
    /// Refreshes every market's session, so opens and closes fire even without price traffic.
    pub(crate) fn refresh_all_sessions(&mut self, events: &mut Vec<Event>) {
        let ids: Vec<MarketId> = self.markets.keys().copied().collect();
        for market_id in ids {
            self.refresh_session(market_id, events);
        }
    }
}

impl Engine {
    /// Whether a market takes no new risk: its own flag, or every market's.
    pub fn is_reduce_only(&self, market: &MarketState) -> bool {
        market.reduce_only || self.global_reduce_only
    }

    /// Updates a market's open flag from its schedule, emitting the transition. Before `V17` an
    /// open cleared the market's move window; from `V17` the window survives the halt, so prices
    /// observed within it before the halt still count after the reopening.
    pub(crate) fn refresh_session(&mut self, market_id: MarketId, events: &mut Vec<Event>) {
        let now = self.now;
        let Some(market) = self.markets.get(&market_id) else { return };
        let open = market.spec.schedule.is_open(now) && !market.corporate_action_blocked;
        if open == market.open {
            return;
        }
        let market = self.markets.get_mut(&market_id).expect("found above");
        market.open = open;
        if open {
            if self.protocol_version < ProtocolVersion::V17 {
                market.stress.move_ref = None;
                market.stress.move_window.clear();
            }
            events.push(Event::MarketOpened { market: market_id });
        } else {
            events.push(Event::MarketClosed { market: market_id });
        }
    }

    /// From `V21`, drops the quotes each market holds from before `last_now`, the previous command's
    /// time, wherever the market was not open throughout since: a close between two commands,
    /// observed or not, leaves no quote from before it, so the first index after an open comes from
    /// quotes stamped after it. Quotes stored by this command passed `MarketState::in_session`, so
    /// none of them predates the closure. From `V33`, a gap longer than the market's freshness window
    /// checks for a closure only within that window after `last_now` and at `now`: later, every quote
    /// held from before the gap is stale and cannot price, at `now` or at a catch-up boundary between.
    pub(super) fn drop_quotes_across_closures(&mut self, last_now: Millis) {
        let now = self.now;
        let bounded = self.protocol_version >= ProtocolVersion::V33;
        let ids: Vec<MarketId> = self
            .markets
            .iter()
            .filter(|(_, market)| {
                let schedule = &market.spec.schedule;
                let window = market.spec.price_rules.max_age_ms;
                market.holds_quotes_to(last_now)
                    && if bounded && now - last_now > window {
                        !(schedule.open_throughout(last_now, last_now + window) && schedule.is_open(now))
                    } else {
                        !schedule.open_throughout(last_now, now)
                    }
            })
            .map(|(id, _)| *id)
            .collect();
        for market_id in ids {
            self.markets.get_mut(&market_id).expect("listed").drop_quotes_to(last_now);
        }
    }
}
