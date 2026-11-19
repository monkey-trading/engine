//! Listings: `UpsertMarket`, which lists or relists one market, and from `V81` `RelistMarkets`, which
//! relists several at once, all or nothing, as an offset group's calendar changes (#913, D3). Each
//! listing passes the same checks, the parameters' offset grid is checked over the listings that result,
//! and only then does any apply; weekend scheduling, the sessions, the markets' processing and the
//! close-out checks follow once every listing has applied.

use std::collections::BTreeSet;

use crate::engine::{Engine, MarketState};
use crate::event::Event;
use crate::futures::FuturesState;
use crate::listing_bounds::ListingChange;
use crate::market::MarketSpec;
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

/// A listing that passed its checks, with what applying it needs.
pub(super) struct CheckedListing {
    change: ListingChange,
    futures: Option<FuturesState>,
    currency_changed: bool,
}

impl Engine {
    /// Lists or relists `spec`'s market (`AdminCommand::UpsertMarket`).
    /// Error cases:
    /// - any error case of `check_listing`, or of `check_grid_fit` over the listing
    pub(super) fn upsert_market(
        &mut self,
        spec: &MarketSpec,
        voted: bool,
        events: &mut Vec<Event>,
    ) -> Result<(), String> {
        let checked = self.check_listing(spec, voted)?;
        self.check_grid_fit(std::slice::from_ref(spec))?;
        let currency_changed = self.apply_listing(spec, checked);
        self.after_listings(&[(spec.id, currency_changed)], events);
        Ok(())
    }

    /// From `V81`, relists every market `specs` names at once (`AdminCommand::RelistMarkets`): each passes
    /// `check_listing` against the listings before the command, the offset grid is checked over all of
    /// them, and then each applies, before weekend scheduling, the sessions, the markets' processing and
    /// the close-out checks run once. Each must keep its market's quote currency and symbol, so no
    /// listing's checks read another's.
    /// Error cases:
    /// - before `V81`
    /// - no listings, a market named twice, or one not listed
    /// - a listing that changes its market's quote currency or symbol
    /// - any error case of `check_listing` for any listing, or of `check_grid_fit` over all of them
    pub(super) fn relist_markets(
        &mut self,
        specs: &[MarketSpec],
        voted: bool,
        events: &mut Vec<Event>,
    ) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V81 {
            return Err(format!("relisting several markets at once needs protocol {:?}", ProtocolVersion::V81));
        }
        if specs.is_empty() {
            return Err("no markets to relist".into());
        }
        let mut named = BTreeSet::new();
        for spec in specs {
            if !named.insert(spec.id) {
                return Err(format!("market {} is named twice", spec.id));
            }
            let listed = &self.markets.get(&spec.id).ok_or_else(|| format!("market {} is not listed", spec.id))?.spec;
            if listed.quote_currency != spec.quote_currency || listed.symbol != spec.symbol {
                return Err(format!(
                    "a relisting of several markets keeps market {}'s quote currency and symbol",
                    spec.id
                ));
            }
        }
        let checked = specs.iter().map(|spec| self.check_listing(spec, voted)).collect::<Result<Vec<_>, _>>()?;
        self.check_grid_fit(specs)?;
        let applied: Vec<(MarketId, bool)> =
            specs.iter().zip(checked).map(|(spec, checked)| (spec.id, self.apply_listing(spec, checked))).collect();
        self.after_listings(&applied, events);
        Ok(())
    }

    /// Checks a listing of `spec` before anything applies. A `voted` listing is a passed vote's.
    /// Error cases:
    /// - a spec that fails `MarketSpec::validate` or `check_quote_currency`, with dated closures or special
    ///   sessions before `V4`, or futures contracts before `V21`, or that `MarketState::relisted` refuses
    ///   for a listed market
    /// - any error case of `check_corporate_listing`, `check_listing_ceilings`, `check_markout_floor`,
    ///   `check_level_distance_listing`, from `V35` `check_priceable`, `check_held_benchmark`,
    ///   `check_weekend_listing` or `check_listing_change`
    /// - from `V13`, a divergence bound no divergence share in force can be reached at
    /// - from `V14`, once any benchmark has a rate, a listing that names a benchmark with none
    pub(super) fn check_listing(&self, spec: &MarketSpec, voted: bool) -> Result<CheckedListing, String> {
        spec.validate()?;
        self.check_corporate_listing(spec)?;
        self.check_listing_ceilings(spec)?;
        self.check_markout_floor(spec)?;
        self.check_level_distance_listing(spec)?;
        if spec.schedule.has_exceptions() && self.protocol_version < ProtocolVersion::V4 {
            return Err(format!("dated closures and special sessions need protocol {:?}", ProtocolVersion::V4));
        }
        if !spec.contracts.is_empty() && self.protocol_version < ProtocolVersion::V21 {
            return Err(format!("futures contracts need protocol {:?}", ProtocolVersion::V21));
        }
        self.check_quote_currency(spec)?;
        if self.protocol_version >= ProtocolVersion::V35 {
            self.check_priceable(spec)?;
        }
        self.check_held_benchmark(spec)?;
        self.check_weekend_listing(spec, voted)?;
        let change = self.check_listing_change(spec, voted)?;
        if let Some(params) = self.params.as_ref().filter(|_| self.protocol_version >= ProtocolVersion::V13) {
            params.check_divergence_reachable(spec)?;
        }
        // Before any benchmark has a rate, as in a genesis that leaves every rate to the feed,
        // a listing may name any; after, a new name must be one with a rate.
        let benchmark = &spec.financing.benchmark;
        let named = self.markets.get(&spec.id).is_none_or(|market| market.spec.financing.benchmark != *benchmark);
        if self.protocol_version >= ProtocolVersion::V14
            && named
            && (!self.benchmarks.is_empty()
                || (self.protocol_version >= ProtocolVersion::V18 && !self.benchmark_history.is_empty()))
            && !self.benchmarks.contains_key(benchmark)
            && !(self.protocol_version >= ProtocolVersion::V18 && self.benchmark_history.contains_key(benchmark))
        {
            return Err(format!("benchmark {benchmark} has no rate"));
        }
        let currency_changed =
            self.markets.get(&spec.id).is_some_and(|market| market.spec.quote_currency != spec.quote_currency);
        let futures = match self.markets.get(&spec.id) {
            Some(market) => market.relisted(spec)?,
            None => FuturesState::listed(spec, self.now),
        };
        Ok(CheckedListing { change, futures, currency_changed })
    }

    /// Checks the parameters' offset grid fits the listings `specs` leave, each in place of its market's
    /// (`PortfolioGrid::check_listings`): a group's listed members on one asset class and one schedule,
    /// quoted in USD.
    /// Error cases:
    /// - as `PortfolioGrid::check_listings`
    fn check_grid_fit(&self, specs: &[MarketSpec]) -> Result<(), String> {
        let Some(params) = &self.params else { return Ok(()) };
        params.portfolio_grid.check_listings(|id| {
            specs.iter().find(|spec| spec.id == id).or_else(|| self.markets.get(&id).map(|market| &market.spec))
        })
    }

    /// Applies a checked listing of `spec`: the market's spec, its futures, and from `V33` the quotes a
    /// raised freshness limit would bring back, then the baseline and the notices the listing spends.
    /// Returns whether it changed the market's quote currency.
    fn apply_listing(&mut self, spec: &MarketSpec, checked: CheckedListing) -> bool {
        let CheckedListing { change, futures, currency_changed } = checked;
        self.end_unpriced_interval(spec.id);
        match self.markets.get_mut(&spec.id) {
            Some(market) => {
                // An FX mark belongs to the currency it was taken for.
                if market.spec.quote_currency != spec.quote_currency {
                    market.fx_mark = None;
                }
                let old_max_age_ms = market.spec.price_rules.max_age_ms;
                market.spec = spec.clone();
                market.futures = futures;
                // From `V33`, a quote stale under the old limit stays stale: a raised limit
                // must not bring it back to pricing.
                if self.protocol_version >= ProtocolVersion::V33 && spec.price_rules.max_age_ms > old_max_age_ms {
                    market.drop_quotes_older_than(self.now, old_max_age_ms);
                }
            }
            None => {
                self.markets.insert(spec.id, MarketState { futures, ..MarketState::new(spec.clone()) });
            }
        }
        self.end_unpriced_interval(spec.id);
        self.rebase(Some(spec.id));
        self.settle_listing_change(spec, change);
        currency_changed
    }

    /// After the listings `applied` names, each market with whether its quote currency changed: each
    /// market's weekend gap, then its session, then the markets' processing, as a price update for a
    /// market others convert at or whose currency changed, and then, from `V6`, the close-out checks of
    /// each market's holders.
    fn after_listings(&mut self, applied: &[(MarketId, bool)], events: &mut Vec<Event>) {
        let ids: Vec<MarketId> = applied.iter().map(|(id, _)| *id).collect();
        self.relist_weekends(&ids, events);
        for (id, _) in applied {
            self.refresh_session(*id, events);
        }
        // A new listing for a market others convert at can change what its index converts at, so it is
        // their price update too, before any close-out runs.
        let reprices = |engine: &Self, id: MarketId| {
            let converts_here =
                |market: &MarketState| market.spec.id != id && market.spec.quote_currency.fx_market() == Some(id);
            engine.protocol_version >= ProtocolVersion::V7
                && engine.params.is_some()
                && engine.markets.values().any(converts_here)
        };
        if self.protocol_version >= ProtocolVersion::V12 {
            // A new quote currency drops the FX mark, which a market that converts at its own index takes
            // back from its own index only when it is processed.
            let forced: Vec<MarketId> =
                applied.iter().filter(|(id, changed)| *changed || reprices(self, *id)).map(|(id, _)| *id).collect();
            if forced.is_empty() {
                self.advance_markets(None, events);
            }
            for id in forced {
                self.advance_markets(Some(id), events);
            }
        } else {
            for (id, _) in applied {
                if reprices(self, *id) {
                    self.reprice(*id, events);
                }
            }
        }
        if self.protocol_version >= ProtocolVersion::V6 && self.params.is_some() {
            for (id, _) in applied {
                self.check_closeouts(*id, events);
            }
        }
    }
}
