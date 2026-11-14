//! The margin and house exposure checks a fill that adds risk must pass.

use std::cell::OnceCell;

use crate::Fixed;
use crate::account::{Account, Position};
use crate::engine::{Engine, MarketState};
use crate::event::Reject;
use crate::execution::validation::profit_fee;
use crate::execution::{Exec, Priced};
use crate::fixed::Rounding;
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

/// The house totals a risk check reads. The NAV is taken at once, and the gross exposure on first
/// use, so a run of checks over one state computes each once.
pub(super) struct HouseTotals {
    nav: Fixed,
    gross: OnceCell<Fixed>,
}

impl HouseTotals {
    pub(super) fn of(engine: &Engine) -> Self {
        Self { nav: engine.house_nav(), gross: OnceCell::new() }
    }

    fn gross(&self, engine: &Engine) -> Fixed {
        *self.gross.get_or_init(|| engine.house_gross_exposure())
    }
}

impl Engine {
    /// Checks a fill that adds risk against the account's margin and the house's exposure caps, in
    /// USD: realised PnL converts rounded down, and exposure rounded up.
    /// Error cases:
    /// - the account's equity after the fill would be below its margin requirement
    /// - from `V59`, below its requirement with the weekend margins that apply now
    /// - the fill would push the house's net exposure past the market cap or the total cap
    pub(super) fn check_risk(&self, exec: &Exec, priced: &Priced, house: &HouseTotals) -> Result<(), Reject> {
        let params = self.params.as_ref().expect("callers check params are set");
        let market = &self.markets[&exec.market];
        let usd = |amount, rounding| market.spec.quote_currency.to_usd(amount, priced.fx, rounding);

        // Margin reads only cash and positions, so the resting orders are not copied.
        let acct = &self.accounts[&exec.account];
        let mut sim = Account { balance: acct.balance, positions: acct.positions.clone(), ..Account::default() };
        let version = self.protocol_version;
        let realised = sim.positions.entry(exec.market).or_default().apply(priced.delta, priced.price, version);
        let realised = usd(realised, Rounding::Down);
        // From `V55` the fee is the closing slice's, at its hold.
        let (_, fee_bps) = self.close_fee_terms(exec.account, exec.market);
        sim.balance += realised - profit_fee(realised, fee_bps, version);
        let (equity, required) = self.equity_and_margin(&sim, Some((exec.market, priced.mid, priced.fx)));
        if equity < required {
            return Err(Reject::InsufficientMargin { equity, required });
        }
        // From `V59`, risk added in any market meets the weekend margin of each market whose window
        // covers now.
        if self.protocol_version >= ProtocolVersion::V59 {
            let price_of = |id: MarketId, other: &MarketState, pos: &Position| {
                if id == exec.market {
                    (priced.mid, priced.fx)
                } else {
                    (other.mark.unwrap_or(pos.entry), other.fx_mark)
                }
            };
            let required = required + self.weekend_margin_extra(&sim, self.now, price_of);
            if equity < required {
                return Err(Reject::WeekendMargin { equity, required });
            }
        }

        let nav = house.nav;
        let before = usd(market.client_qty.abs().mul(priced.mid), Rounding::Up);
        let after = usd((market.client_qty + priced.delta).abs().mul(priced.mid), Rounding::Up);
        if after > before {
            // `house_gross_exposure` values this market at its marks; swap that term for `after`.
            let exposure = |mark: Fixed| market.usd_at_mark(market.client_qty.abs().mul(mark), Rounding::Up);
            let at_mark = market.mark.map_or(Fixed::ZERO, exposure);
            let total_after = house.gross(self) - at_mark + after;
            if after > self.market_cap(&market.spec, nav) || total_after > self.total_cap(params, nav) {
                return Err(Reject::HouseCapacity);
            }
        }
        Ok(())
    }
}
