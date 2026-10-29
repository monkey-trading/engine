//! The checks an order passes before it is considered at all.

use crate::Fixed;
use crate::account::Account;
use crate::command::{OrderKind, OrderRequest};
use crate::engine::{Engine, MarketState};
use crate::event::Reject;
use crate::execution::growth;
use crate::fixed::Rounding;
use crate::params::ProtocolParams;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId};

impl Engine {
    /// Checks an order can be considered at all.
    /// Error cases:
    /// - protocol params not set yet
    /// - an unknown market
    /// - a qty, quoted price, or trigger price at or below zero or above `Fixed::MAX_INPUT`
    /// - an account that has never been credited
    /// - from `V14`, a limit or stop order from an account already resting the parameters' cap, or
    ///   one whose notional at its own price, in USD at that price in a market that converts at its
    ///   own index and at its FX market's last mark in any other, is below the market's minimum, or
    ///   whose FX market has no mark yet; a reduce-only order is never checked, and from `V15`
    ///   neither is one that can only reduce the account's position in that market — opposite side,
    ///   quantity at most the position
    pub(super) fn validate_order(&self, req: &OrderRequest) -> Result<(), Reject> {
        let invalid = |reason: &str| Err(Reject::InvalidOrder { reason: reason.into() });
        let (price, rests) = match req.kind {
            OrderKind::Market { quoted, .. } => (quoted, false),
            OrderKind::Limit { price } | OrderKind::Stop { price } => (price, true),
        };
        let (params, market, acct) = self.orderable(req.account, req.market, req.qty, Some(price))?;
        if rests && self.protocol_version >= ProtocolVersion::V14 {
            if acct.orders.len() >= params.max_resting_orders {
                return Err(Reject::TooManyOrders { max: params.max_resting_orders });
            }
            // A reduce-only order never adds risk, as a reducing fill pays no minimum; from `V15`
            // neither does an order that can only reduce the account's position in this market. A
            // fill checks the minimum only on the slice that grows |position| (`price`), so the
            // exemption is the same `growth` slice with nothing growing. The order is valued at its
            // own price, converted as its fill would be: a market that converts at its own index, as
            // USD-JPY does, at that price, and any other at its FX market's last mark.
            let reduces_only = req.reduce_only
                || (self.protocol_version >= ProtocolVersion::V15 && {
                    let pos = acct.position(req.market);
                    let (from, to) = growth(pos.qty, req.side.signed(req.qty));
                    to <= from
                });
            if !reduces_only {
                let currency = market.spec.quote_currency;
                // Listing checks the FX market is listed, and no market is ever removed.
                let fx_at = |fx_market| {
                    if fx_market == req.market {
                        Ok(price)
                    } else {
                        self.markets[&fx_market].mark.ok_or(Reject::NoFxPrice { fx_market })
                    }
                };
                let fx = currency.fx_market().map(fx_at).transpose()?;
                if currency.to_usd(req.qty.mul(price), fx, Rounding::Down) < market.spec.min_order_notional {
                    return invalid("below minimum order notional");
                }
            }
        }
        Ok(())
    }

    /// The parameters, market, and account an order is checked against, once its market is listed,
    /// its qty and any price are in range, and its account has been credited.
    /// Error cases:
    /// - protocol params not set yet
    /// - an unknown market
    /// - a qty, or a quoted or trigger price, at or below zero or above `Fixed::MAX_INPUT`
    /// - an account that has never been credited
    pub(super) fn orderable(
        &self,
        account: AccountId,
        market: MarketId,
        qty: Fixed,
        price: Option<Fixed>,
    ) -> Result<(&ProtocolParams, &MarketState, &Account), Reject> {
        let invalid = |reason: &str| Err(Reject::InvalidOrder { reason: reason.into() });
        let Some(params) = &self.params else {
            return invalid("protocol params not set");
        };
        let Some(market) = self.markets.get(&market) else {
            return Err(Reject::UnknownMarket);
        };
        if !qty.is_valid_input() {
            return invalid("qty out of range");
        }
        if price.is_some_and(|price| !price.is_valid_input()) {
            return invalid("price out of range");
        }
        let Some(acct) = self.accounts.get(&account) else {
            return Err(Reject::InsufficientFunds { available: Fixed::ZERO });
        };
        Ok((params, market, acct))
    }
}
