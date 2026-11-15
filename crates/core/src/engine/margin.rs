//! An account's equity, margin requirement and, from `V64`, initial requirement at marks.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::Fixed;
use crate::account::{Account, Position};
use crate::engine::{Engine, MarketState};
use crate::fixed::Rounding;
use crate::types::{AccountId, MarketId};

/// An account's initial and maintenance requirements, withdrawable cash and maximum leverage in each
/// market, as the account read gives them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MarginStanding {
    /// What a fill that adds risk, and a withdrawal, must leave equity at or above, at marks now:
    /// from `V64` each margin band charged at least the account's tier's floor, and from `V60` each
    /// position in a market whose weekend margin applies now at least that, band by band.
    pub initial_margin: Fixed,
    /// The equity below which a price move closes the account out, at marks now: the requirement at
    /// the market's bands, at the close-out level. A weekend gap's start closes out below what
    /// `Engine::weekend_requirements` gives instead.
    pub maintenance_margin: Fixed,
    /// The cash a withdrawal may take now (`Engine::withdrawable`).
    pub withdrawable: Fixed,
    /// The most leverage a position opened now can take or approach in each listed market, its margin
    /// bands charged as `initial_margin` charges them (`MarketSpec::max_leverage`).
    pub max_leverage: BTreeMap<MarketId, Fixed>,
}

impl Engine {
    /// (equity, margin requirement) in USD at current marks, optionally overriding one market's mark
    /// and FX index.
    pub fn equity_and_margin(
        &self,
        acct: &Account,
        mark_override: Option<(MarketId, Fixed, Option<Fixed>)>,
    ) -> (Fixed, Fixed) {
        self.equity_and_initial_margin(acct, 0, mark_override)
    }

    /// (equity, initial requirement) in USD at current marks, optionally overriding one market's mark
    /// and FX index: each margin band charges at least `floor_bps`, the margin floor of the account's
    /// tier (`Engine::margin_floor_bps`). At a floor of zero, the margin requirement.
    pub(crate) fn equity_and_initial_margin(
        &self,
        acct: &Account,
        floor_bps: i64,
        mark_override: Option<(MarketId, Fixed, Option<Fixed>)>,
    ) -> (Fixed, Fixed) {
        self.equity_and_requirement_at(acct, floor_bps, |market_id, market, pos| match mark_override {
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
        self.equity_and_requirement_at(acct, 0, price_of)
    }

    /// `equity_and_margin_at` with each margin band charging at least `floor_bps`.
    fn equity_and_requirement_at(
        &self,
        acct: &Account,
        floor_bps: i64,
        price_of: impl Fn(MarketId, &MarketState, &Position) -> (Fixed, Option<Fixed>),
    ) -> (Fixed, Fixed) {
        let charge = self.charge_rounding();
        acct.positions
            .iter()
            .filter_map(|(market_id, pos)| self.markets.get(market_id).map(|market| (*market_id, pos, market)))
            .fold((acct.balance, Fixed::ZERO), |(equity, margin), (market_id, pos, market)| {
                let (price, fx) = price_of(market_id, market, pos);
                let usd = |amount, rounding| market.spec.quote_currency.to_usd(amount, fx, rounding);
                let notional = usd(pos.qty.abs().mul_rounded(price, charge), Rounding::Up);
                let pnl = usd(pos.unrealised(price, self.protocol_version), Rounding::Down);
                (equity + pnl, margin + market.spec.floored_margin_for(notional, floor_bps, charge))
            })
    }

    /// (equity, initial requirement) of `acct`, `account`'s, at marks now: each margin band charged at
    /// least its tier's floor, and from `V60` each position in a market whose weekend margin applies
    /// now at least that too.
    pub(crate) fn equity_and_initial_margin_now(&self, account: AccountId, acct: &Account) -> (Fixed, Fixed) {
        let floor_bps = self.margin_floor_bps(account);
        let (equity, initial) = self.equity_and_initial_margin(acct, floor_bps, None);
        (equity, initial + self.weekend_margin_extra_now(acct, floor_bps))
    }

    /// `account`'s initial and maintenance requirements, the cash it may withdraw, and the most
    /// leverage it opens at in each listed market, at marks now; `None` for an account never credited.
    pub fn margin_standing(&self, account: AccountId) -> Option<MarginStanding> {
        let acct = self.accounts.get(&account)?;
        let (_, initial_margin) = self.equity_and_initial_margin_now(account, acct);
        let (_, margin) = self.equity_and_margin(acct, None);
        let maintenance_margin = match &self.params {
            Some(params) => margin.mul_bps_rounded(params.closeout_level_bps, self.charge_rounding()),
            None => margin,
        };
        let floor_bps = self.margin_floor_bps(account);
        let max_leverage = self
            .markets
            .iter()
            .map(|(id, market)| {
                let weekend = if market.weekend_margin_applies(self.now) { market.spec.weekend_margin_bps } else { 0 };
                (*id, market.spec.max_leverage(floor_bps.max(weekend)))
            })
            .collect();
        Some(MarginStanding {
            initial_margin,
            maintenance_margin,
            withdrawable: self.withdrawable(account),
            max_leverage,
        })
    }
}
