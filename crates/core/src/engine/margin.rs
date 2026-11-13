//! An account's equity and margin requirement at marks.

use crate::Fixed;
use crate::account::{Account, Position};
use crate::engine::{Engine, MarketState};
use crate::fixed::Rounding;
use crate::types::MarketId;

impl Engine {
    /// (equity, margin requirement) in USD at current marks, optionally overriding one market's mark
    /// and FX index.
    pub fn equity_and_margin(
        &self,
        acct: &Account,
        mark_override: Option<(MarketId, Fixed, Option<Fixed>)>,
    ) -> (Fixed, Fixed) {
        self.equity_and_margin_at(acct, |market_id, market, pos| match mark_override {
            Some((id, mid, fx)) if id == market_id => (mid, fx),
            _ => (market.mark.unwrap_or(pos.entry), market.fx_mark),
        })
    }

    /// (equity, margin requirement) in USD with each position valued at the price and FX index
    /// `price_of(market id, market, position)` gives: PnL converts rounded down and notional rounded
    /// up. From `V10` the requirement rounds up, from each position's notional to each band's charge.
    pub(crate) fn equity_and_margin_at(
        &self,
        acct: &Account,
        price_of: impl Fn(MarketId, &MarketState, &Position) -> (Fixed, Option<Fixed>),
    ) -> (Fixed, Fixed) {
        acct.positions
            .iter()
            .filter_map(|(market_id, pos)| self.markets.get(market_id).map(|market| (*market_id, pos, market)))
            .fold((acct.balance, Fixed::ZERO), |(equity, margin), (market_id, pos, market)| {
                let (price, fx) = price_of(market_id, market, pos);
                let usd = |amount, rounding| market.spec.quote_currency.to_usd(amount, fx, rounding);
                let notional = usd(pos.qty.abs().mul_rounded(price, self.charge_rounding()), Rounding::Up);
                let pnl = usd(pos.unrealised(price, self.protocol_version), Rounding::Down);
                (equity + pnl, margin + market.spec.margin_for(notional, self.charge_rounding()))
            })
    }
}
