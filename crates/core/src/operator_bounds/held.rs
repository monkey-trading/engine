//! Bounds on relisting a market that holds positions: its price rules leave it priced, and it stays on
//! its benchmark.

use crate::engine::Engine;
use crate::market::MarketSpec;
use crate::protocol::ProtocolVersion;

impl Engine {
    /// Checks a relisting leaves priced the markets that hold positions and depend on its price: the
    /// market itself, and the markets that convert at it. Price rules no tighter than those in force
    /// (no more sources, no shorter freshness, no narrower divergence) always do. Tighter ones must
    /// still give the market an index, from the quotes it keeps through the relisting, as trading
    /// reads it, unless every affected market that holds positions is reduce-only.
    /// Error cases:
    /// - tighter price rules that give no index, in a market that holds positions and accepts new
    ///   risk, or that markets holding positions and accepting new risk convert at
    pub(crate) fn check_priceable(&self, spec: &MarketSpec) -> Result<(), String> {
        let Some(market) = self.markets.get(&spec.id) else { return Ok(()) };
        let (old, new) = (&market.spec.price_rules, &spec.price_rules);
        if !old.looser_than(new) {
            return Ok(());
        }
        // The market as the relisting leaves it: `UpsertMarket` drops the quotes stale under the old
        // limit when a relisting raises it.
        let mut relisted = market.clone();
        relisted.spec = spec.clone();
        if self.protocol_version >= ProtocolVersion::V33 && new.max_age_ms > old.max_age_ms {
            relisted.drop_quotes_older_than(self.now, old.max_age_ms);
        }
        let index = relisted.index(self.now).ok();
        let holders = &self.hot_indexes.as_ref().expect("apply builds derived indexes").holders;
        for affected in self.markets.values() {
            let own = affected.spec.id == spec.id;
            let converts = affected.spec.quote_currency.fx_market() == Some(spec.id);
            if !own && !converts {
                continue;
            }
            // No index prices nothing; an index too high to convert a currency prices nothing in it.
            let unpriced = match index {
                None => true,
                Some(index) => converts && !affected.spec.quote_currency.converts_at(index.mid),
            };
            let held = holders.get(&affected.spec.id).is_some_and(|accounts| !accounts.is_empty());
            if !unpriced || !held || self.is_reduce_only(affected) {
                continue;
            }
            return Err(if own {
                format!(
                    "market {} has open positions, so price rules it cannot price under need it reduce-only first",
                    spec.id
                )
            } else {
                format!(
                    "market {} has open positions and converts at market {}, which the price rules cannot price under, so it needs to be reduce-only first",
                    affected.spec.id, spec.id
                )
            });
        }
        Ok(())
    }

    /// From `V46`, checks a relisting leaves a market that holds positions on its benchmark: a first
    /// rate is bound only by its range, so a new name would reach those positions past the cap. No
    /// limit on the move would do, since fresh names walk any such limit within one second.
    /// Error cases:
    /// - a different benchmark, in a market that holds positions
    pub(crate) fn check_held_benchmark(&self, spec: &MarketSpec) -> Result<(), String> {
        let Some(market) = self.markets.get(&spec.id) else { return Ok(()) };
        let current = &market.spec.financing.benchmark;
        if self.protocol_version < ProtocolVersion::V46 || *current == spec.financing.benchmark {
            return Ok(());
        }
        let holders = &self.hot_indexes.as_ref().expect("apply builds derived indexes").holders;
        if holders.get(&spec.id).is_some_and(|accounts| !accounts.is_empty()) {
            return Err(format!("market {} has open positions, so it stays on benchmark {current}", spec.id));
        }
        Ok(())
    }
}
