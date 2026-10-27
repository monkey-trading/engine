//! House NAV, exposure, and the caps set from them.

use crate::Fixed;
use crate::engine::Engine;
use crate::fixed::Rounding;
use crate::market::MarketSpec;
use crate::params::ProtocolParams;
use crate::protocol::ProtocolVersion;

impl Engine {
    /// House cash, before clients' unrealised PnL.
    pub fn house_cash(&self) -> Fixed {
        self.house_cash
    }

    /// House cash minus clients' unrealised PnL at current marks, in USD. Clients' PnL rounds up, so
    /// NAV, and every cap set from it, rounds down.
    pub fn house_nav(&self) -> Fixed {
        let client_upnl: Fixed = self
            .markets
            .values()
            .filter_map(|market| {
                let upnl = |mark: Fixed| mark.mul(market.client_qty) - market.client_cost;
                market.mark.map(|mark| market.usd_at_mark(upnl(mark), Rounding::Up))
            })
            .sum();
        self.house_cash - client_upnl
    }

    /// Sum over markets of the house's absolute net notional, in USD, each rounded up.
    pub fn house_gross_exposure(&self) -> Fixed {
        self.markets
            .values()
            .filter_map(|market| {
                market.mark.map(|mark| market.usd_at_mark(market.client_qty.abs().mul(mark), Rounding::Up))
            })
            .sum()
    }

    /// The cap on the house's absolute net notional in `spec`'s market at house NAV `nav`: its share
    /// of NAV, or from `V6` the larger of that and the market's floor.
    pub(crate) fn market_cap(&self, spec: &MarketSpec, nav: Fixed) -> Fixed {
        self.floored(nav.mul_bps(spec.house_cap_bps), spec.house_cap_floor)
    }

    /// The cap on the house's gross exposure across markets at house NAV `nav`: its share of NAV,
    /// or from `V6` the larger of that and the total floor.
    pub(crate) fn total_cap(&self, params: &ProtocolParams, nav: Fixed) -> Fixed {
        self.floored(nav.mul_bps(params.house_total_cap_bps), params.house_total_cap_floor)
    }

    /// `share`, or from `V6` the larger of `share` and `floor`.
    fn floored(&self, share: Fixed, floor: Fixed) -> Fixed {
        if self.protocol_version >= ProtocolVersion::V6 { share.max(floor) } else { share }
    }
}
