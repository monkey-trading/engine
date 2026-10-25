//! Checked cash transfers in USD at the same recorded FX marks used for financing valuation.

use crate::Fixed;
use crate::corporate::{Adjustment, CorporateEntitlement};
use crate::engine::{Engine, MarketState};
use crate::event::Event;
use crate::fixed::Rounding;
use crate::market::QuoteCurrency;

impl Engine {
    /// Preflights every debit, credit and house offset, then commits the dividend atomically.
    /// Error cases:
    /// - missing/unusable FX mark, missing entitlement, or an unrepresentable transfer/balance
    pub(super) fn apply_corporate_dividend(
        &mut self,
        pending: &CorporateEntitlement,
        events: &mut Vec<Event>,
    ) -> Result<(), String> {
        let action = &pending.instruction;
        let Adjustment::Dividend { gross_per_unit: gross, withholding_rate } = action.adjustment else {
            unreachable!("dividend dispatch checks adjustment");
        };
        let quantities = pending.quantities.as_ref().ok_or("dividend has no previous-close snapshot")?;
        let market = &self.markets[&action.market];
        let currency = market.spec.quote_currency;
        let fx = valuation_fx(market).map_err(|reason| format!("dividend {reason}"))?;
        let mut house = self.house_cash.raw();
        let mut transfers = Vec::with_capacity(quantities.len());
        for (account, quantity) in quantities {
            // Every step rounds the signed account credit down: positive credits toward zero,
            // negative debits away from zero, including the final currency conversion.
            let amount =
                quantity.checked_mul_div(gross, Fixed::ONE, Rounding::Down).ok_or("dividend gross amount overflows")?;
            let amount = if quantity.is_positive() {
                amount
                    .checked_mul_div(Fixed::ONE - withholding_rate, Fixed::ONE, Rounding::Down)
                    .ok_or("dividend net amount overflows")?
            } else {
                amount
            };
            let amount = usd_down(currency, amount, fx)?;
            let balance = self
                .accounts
                .get(account)
                .ok_or("entitled account is missing")?
                .balance
                .raw()
                .checked_add(amount.raw())
                .ok_or("dividend account balance overflows")?;
            house = house.checked_sub(amount.raw()).ok_or("dividend house balance overflows")?;
            transfers.push((*account, *quantity, amount, Fixed::from_raw(balance)));
        }
        for (account, quantity, amount, balance) in transfers {
            self.accounts.get_mut(&account).expect("preflight checked account").balance = balance;
            events.push(Event::CorporateDividend {
                id: action.id.clone(),
                market: action.market,
                account,
                quantity,
                amount,
                fx_mark: fx,
            });
        }
        self.house_cash = Fixed::from_raw(house);
        Ok(())
    }
}

/// The market's recorded FX valuation mark, the one financing values at, or `None` for USD.
/// Error cases:
/// - a non-USD market without a positive recorded mark it converts at
pub(super) fn valuation_fx(market: &MarketState) -> Result<Option<Fixed>, String> {
    let currency = market.spec.quote_currency;
    match market.fx_mark {
        _ if currency.is_usd() => Ok(None),
        Some(fx) if fx.is_positive() && currency.converts_at(fx) => Ok(Some(fx)),
        _ => Err("requires a recorded usable FX valuation mark".into()),
    }
}

/// A signed quote-currency amount in USD at `fx` from [`valuation_fx`], rounded down: a credit
/// toward zero and a debit away from it.
/// Error cases:
/// - a non-USD amount has no FX mark, or the converted amount is unrepresentable
pub(super) fn usd_down(currency: QuoteCurrency, amount: Fixed, fx: Option<Fixed>) -> Result<Fixed, String> {
    match currency {
        QuoteCurrency::Usd => Some(amount),
        QuoteCurrency::Eur { .. } | QuoteCurrency::Gbp { .. } => {
            fx.and_then(|fx| amount.checked_mul_div(fx, Fixed::ONE, Rounding::Down))
        }
        QuoteCurrency::Jpy { .. } => fx.and_then(|fx| amount.checked_mul_div(Fixed::ONE, fx, Rounding::Down)),
    }
    .ok_or_else(|| "corporate USD conversion has no FX mark or overflows".into())
}
