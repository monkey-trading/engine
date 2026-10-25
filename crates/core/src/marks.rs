//! Venue prices and marks: storing a market's quotes, and marking it and the markets that convert
//! at its index from them.

use crate::Fixed;
use crate::corporate::Adjustment;
use crate::engine::Engine;
use crate::event::Event;
use crate::market::QuoteCurrency;
use crate::price::{Index, MAX_QUOTE_LEAD_MS, SourceQuote};
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

impl Engine {
    /// Before `V12`, stores venue quotes, then marks the market from them (`reprice`). From `V12`,
    /// `apply` stores them and brings every market to the sequenced time instead.
    pub(crate) fn on_prices(&mut self, market_id: MarketId, quotes: &[SourceQuote], events: &mut Vec<Event>) {
        if self.store_quotes(market_id, quotes, events) {
            self.reprice(market_id, events);
        }
    }

    /// Stores venue quotes, refusing one by one the malformed ones, those stamped more than
    /// `MAX_QUOTE_LEAD_MS` after the sequenced time, and those for a futures contract the market
    /// does not take (`MarketState::quotes_for`); a quote for the contract the market has just
    /// rolled off is dropped without a rejection, as feeds send it until they learn of the roll.
    /// From `V12`, a quote stamped ahead is stored as stamped at the sequenced time. From `V21`, a
    /// quote is refused unless its market stays open from its venue stamp to the sequenced time
    /// (`MarketState::in_session`), so no quote from outside the session it prices ever counts.
    /// Returns whether the market is listed.
    pub(crate) fn store_quotes(
        &mut self,
        market_id: MarketId,
        quotes: &[SourceQuote],
        events: &mut Vec<Event>,
    ) -> bool {
        let now = self.now;
        let v12 = self.protocol_version >= ProtocolVersion::V12;
        let v21 = self.protocol_version >= ProtocolVersion::V21;
        let pending_split = self
            .corporate_actions
            .values()
            .filter(|p| {
                p.instruction.market == market_id
                    && p.failure.is_none()
                    && matches!(p.instruction.adjustment, Adjustment::Split { .. })
                    && p.instruction.ex_open > now
            })
            .map(|p| (p.instruction.previous_close, p.instruction.ex_open))
            .min();
        let Some(market) = self.markets.get_mut(&market_id) else {
            events.push(Event::PricesRejected { market: market_id, reason: "unknown market".into() });
            return false;
        };
        if market.corporate_action_blocked {
            events.push(Event::PricesRejected {
                market: market_id,
                reason: "quote units are blocked by a failed split".into(),
            });
            return false;
        }
        for quote in quotes {
            if pending_split.is_some_and(|(close, open)| now >= close || quote.ts >= open) {
                events.push(Event::PricesRejected {
                    market: market_id,
                    reason: "quote units are ambiguous during a pending split closure".into(),
                });
                continue;
            }
            if market.last_split_at.is_some_and(|at| quote.ts < at) {
                events
                    .push(Event::PricesRejected { market: market_id, reason: "quote predates the last split".into() });
                continue;
            }
            if !quote.is_well_formed() {
                let reason = format!("malformed quote from {}", quote.source);
                events.push(Event::PricesRejected { market: market_id, reason });
                continue;
            }
            let lead_ms = quote.ts.saturating_sub(now);
            if lead_ms > MAX_QUOTE_LEAD_MS {
                let reason = format!("quote from {} stamped {lead_ms}ms ahead of the sequenced time", quote.source);
                events.push(Event::PricesRejected { market: market_id, reason });
                continue;
            }
            if v21 && !market.in_session(quote.ts, now) {
                let reason = format!("quote from {} stamped or sequenced outside one open session", quote.source);
                events.push(Event::PricesRejected { market: market_id, reason });
                continue;
            }
            // From `V12`, a quote stamped ahead counts from the sequenced time it arrived by, so a
            // venue whose clock leads still counts.
            let quote = SourceQuote { ts: if v12 { quote.ts.min(now) } else { quote.ts }, ..quote.clone() };
            if market.rolled_off(quote.contract.as_deref()) {
                continue;
            }
            let Some(held) = market.quotes_for_mut(quote.contract.as_deref()) else {
                let contract = quote.contract.as_deref().unwrap_or("none");
                let reason =
                    format!("quote from {} for contract {contract}, which the market does not take", quote.source);
                events.push(Event::PricesRejected { market: market_id, reason });
                continue;
            };
            // A stored quote stamped after the sequenced time does not count yet, so it never holds
            // back a replacement.
            if held.get(&quote.source).is_none_or(|old| quote.ts >= old.ts || old.ts > now) {
                held.insert(quote.source.clone(), quote);
            }
        }
        true
    }

    /// Marks a market from its stored quotes, as a price update does: if it is open with a valid
    /// index, processes it at that index (`process_indices`).
    pub(crate) fn reprice(&mut self, market_id: MarketId, events: &mut Vec<Event>) {
        self.refresh_session(market_id, events);
        let market = &self.markets[&market_id];
        if !market.open || self.params.is_none() {
            return;
        }
        let Ok(index) = market.index(self.now) else { return };
        self.process_indices(&[(market_id, index)], events);
    }

    /// From `V12`, brings every market to the sequenced time: refreshes every session, then
    /// processes each market whose index differs from the one it was last processed at, as when it
    /// opens, a quote ages out, its FX market gains or loses a price, or its listing or the
    /// parameters change, and `priced`, whose quotes a price command has just stored or whose
    /// listing others convert at, whenever it has an index, as a price update does. From `V21`,
    /// markets priced from futures roll first where a roll is due. Its cost per command grows with
    /// the listed markets, which only operator commands add.
    pub(crate) fn advance_markets(&mut self, priced: Option<MarketId>, events: &mut Vec<Event>) {
        self.refresh_all_sessions(events);
        if self.protocol_version >= ProtocolVersion::V21 {
            self.roll_futures(events);
        }
        let ids: Vec<MarketId> = self.markets.keys().copied().collect();
        let changed: Vec<(MarketId, Index)> = ids
            .into_iter()
            .filter_map(|market_id| {
                let index = self.live_index(market_id);
                if self.markets[&market_id].processed_index != index {
                    // Taken for change only when it moves, so the state root rehashes those alone.
                    self.markets.get_mut(&market_id).expect("listed above").processed_index = index;
                } else if priced != Some(market_id) {
                    return None;
                }
                index.map(|index| (market_id, index))
            })
            .collect();
        self.process_indices(&changed, events);
    }

    /// The index a fill in the market would use at the sequenced time, from its own quotes, or
    /// `None` while it is closed, before parameters are set, while its quotes give no valid index,
    /// or while its FX market has no price. A market that converts at its own index has an FX price
    /// whenever that index converts, as processing it takes its FX mark from that index.
    pub(crate) fn live_index(&self, market_id: MarketId) -> Option<Index> {
        let market = &self.markets[&market_id];
        if !market.open || self.params.is_none() {
            return None;
        }
        let index = market.index(self.now).ok()?;
        let currency = market.spec.quote_currency;
        let fx_priced = if currency.fx_market() == Some(market_id) {
            currency.converts_at(index.mid)
        } else {
            self.live_fx(market_id).is_ok()
        };
        fx_priced.then_some(index)
    }

    /// Marks markets at new indices, as a price update does: each index becomes the FX mark of the
    /// markets that convert at it, and each market in `indices`, and each other one whose FX mark
    /// moved and that has a price from its own quotes, takes its mark, unless its FX market has no
    /// price. Each step runs over every market before the next: only once every mark is set does
    /// each marked market update its stress (before `V13`), then trigger its resting orders, and
    /// then close-outs run in every market whose value moved.
    fn process_indices(&mut self, indices: &[(MarketId, Index)], events: &mut Vec<Event>) {
        let ids: Vec<MarketId> = indices.iter().map(|(id, _)| *id).collect();
        // The markets each index converts, apart from the FX market itself: those outside `indices`
        // are marked below from their own quotes, and those in it already have their index.
        let mut dependents: Vec<MarketId> = Vec::new();
        let mut fx_moved: Vec<MarketId> = Vec::new();
        for (market_id, index) in indices {
            for dependent in self.mark_fx(*market_id, index.mid) {
                let moved = if ids.contains(&dependent) { &mut fx_moved } else { &mut dependents };
                if dependent != *market_id && !moved.contains(&dependent) {
                    moved.push(dependent);
                }
            }
        }
        let mut marked: Vec<(MarketId, Index)> =
            indices.iter().copied().filter(|(id, index)| self.set_mark(*id, *index)).collect();
        for &dependent in &dependents {
            self.refresh_session(dependent, events);
            if let Some(index) = self.live_index(dependent)
                && self.set_mark(dependent, index)
            {
                marked.push((dependent, index));
            }
        }
        // From `V12`, a market an FX index made dealable is processed at its index as of now.
        if self.protocol_version >= ProtocolVersion::V12 {
            for (id, index) in &marked {
                if self.markets[id].processed_index != Some(*index) {
                    self.markets.get_mut(id).expect("marked above").processed_index = Some(*index);
                }
            }
        }
        // From V13 each fill and the end of each command re-evaluate stress instead.
        if self.protocol_version < ProtocolVersion::V13 {
            for (id, index) in &marked {
                self.update_stress(*id, index.mid, index.divergence_bps, events);
            }
        }
        for (id, _) in &marked {
            self.trigger_resting(*id, events);
        }
        // A new FX mark moves the USD value of every market that took it, marked or not.
        let moved = |id: &MarketId| marked.iter().any(|(m, _)| m == id) || fx_moved.contains(id);
        let closeouts: Vec<MarketId> = ids.iter().copied().filter(moved).chain(dependents.iter().copied()).collect();
        for market_id in closeouts {
            self.check_closeouts(market_id, events);
        }
    }

    /// Marks a market at `index`, its live index, and returns true, unless its FX market has no
    /// price: then it has none either, keeps its last mark, and this returns false.
    fn set_mark(&mut self, market_id: MarketId, index: Index) -> bool {
        if self.live_fx(market_id).is_err() {
            return false;
        }
        self.markets.get_mut(&market_id).expect("market exists").mark = Some(index.mid);
        true
    }

    /// Sets `mid` as the FX mark of every market that converts at `fx_market`'s index and that `mid`
    /// can convert, and returns those markets.
    fn mark_fx(&mut self, fx_market: MarketId, mid: Fixed) -> Vec<MarketId> {
        let converts = |currency: QuoteCurrency| currency.fx_market() == Some(fx_market) && currency.converts_at(mid);
        let marked: Vec<MarketId> =
            self.markets.iter().filter(|(_, m)| converts(m.spec.quote_currency)).map(|(id, _)| *id).collect();
        // Only the markets it marks are taken for change, so the state root rehashes those alone.
        marked.iter().for_each(|id| self.markets.get_mut(id).expect("found above").fx_mark = Some(mid));
        marked
    }
}
