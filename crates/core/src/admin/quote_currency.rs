//! A listing's quote currency, checked against the protocol version and the markets already listed.

use crate::engine::Engine;
use crate::market::MarketSpec;
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

impl Engine {
    /// Checks a listing's quote currency against the protocol version and the markets already listed.
    /// Before `V7` every market is quoted in USD, whatever its symbol.
    /// Error cases:
    /// - a quote currency other than USD before `V7`
    /// - from `V7`, a listing that fails `MarketSpec::check_fx_market`, or that makes a market which
    ///   converts at it fail it
    /// - a change of quote currency while an account holds a position in the market
    pub(super) fn check_quote_currency(&self, spec: &MarketSpec) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V7 {
            if spec.quote_currency.is_usd() {
                return Ok(());
            }
            return Err(format!("a quote currency other than USD needs protocol {:?}", ProtocolVersion::V7));
        }
        let symbol = |id: MarketId| {
            if id == spec.id {
                Some(spec.symbol.as_str())
            } else {
                self.markets.get(&id).map(|market| market.spec.symbol.as_str())
            }
        };
        spec.check_fx_market(symbol)?;
        // A market others convert at must stay their pair.
        let dependents = self.markets.values().map(|market| &market.spec);
        for dependent in dependents.filter(|d| d.id != spec.id && d.quote_currency.fx_market() == Some(spec.id)) {
            dependent.check_fx_market(symbol).map_err(|e| format!("market {} converts here: {e}", dependent.id))?;
        }
        let changed =
            self.markets.get(&spec.id).is_some_and(|market| market.spec.quote_currency != spec.quote_currency);
        if changed && self.accounts.values().any(|acct| acct.positions.contains_key(&spec.id)) {
            return Err(format!("market {} has open positions, so its quote currency cannot change", spec.id));
        }
        Ok(())
    }
}
