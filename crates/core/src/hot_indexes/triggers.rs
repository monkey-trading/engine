//! The trigger index: each market's resting orders by the index mid at which a price first reaches
//! them. A stop is keyed at its price. From `V74` a limit is keyed where a fill paying its market's
//! least fill rate (`Engine::least_fill_rate`) first reaches it, so no price it could fill at leaves it
//! out, and each market's limits are re-keyed when that rate moves.

use std::collections::BTreeMap;

use crate::Fixed;
use crate::account::RestingOrder;
use crate::command::OrderKind;
use crate::engine::Engine;
use crate::hot_indexes::HotIndexes;
use crate::types::{AccountId, MarketId, OrderId, Side};

/// Tenths of a bp in 100%.
const WHOLE_TENTH_BPS: i128 = 100_000;

#[derive(Debug, Default)]
pub(super) struct Triggers {
    below: BTreeMap<(Fixed, OrderId), AccountId>,
    above: BTreeMap<(Fixed, OrderId), AccountId>,
    /// The least fill rate, in tenths of a bp, the limits are keyed at.
    rate: i64,
}

impl HotIndexes {
    /// The triggered subset in the same time priority as the authoritative resting book.
    pub(crate) fn triggered(&self, market: MarketId, mid: Fixed) -> Vec<(OrderId, AccountId)> {
        let Some(book) = self.triggers.get(&market) else { return Vec::new() };
        let mut orders: Vec<_> = book
            .below
            .range(..=(mid, OrderId::MAX))
            .chain(book.above.range((mid, 0)..))
            .map(|((_, id), account)| (*id, *account))
            .collect();
        orders.sort_unstable_by_key(|(id, _)| *id);
        orders
    }

    /// Re-keys `market`'s limits at `rate` from `orders`, its whole resting book, unless they are keyed
    /// at it already.
    pub(super) fn key_triggers<'a>(
        &mut self,
        market: MarketId,
        rate: i64,
        orders: impl Iterator<Item = (AccountId, &'a RestingOrder)>,
    ) {
        let book = self.triggers.entry(market).or_default();
        if book.rate == rate {
            return;
        }
        *book = Triggers { rate, ..Triggers::default() };
        orders.for_each(|(account, order)| self.order(account, order, true));
    }

    /// Adds or removes an order at its key: a sell limit and a buy stop trigger once the mid is at or
    /// above it, a buy limit and a sell stop once it is at or below it.
    pub(super) fn order(&mut self, account: AccountId, order: &RestingOrder, insert: bool) {
        let book = self.triggers.entry(order.market).or_default();
        let (key, below) = match (order.kind, order.side) {
            (OrderKind::Limit { price }, side) => (reach(price, side, book.rate), side == Side::Sell),
            (OrderKind::Stop { price }, side) => (price, side == Side::Buy),
            (OrderKind::Market { .. }, _) => return,
        };
        let index = if below { &mut book.below } else { &mut book.above };
        if insert {
            index.insert((key, order.order_id), account);
        } else {
            index.remove(&(key, order.order_id));
        }
    }
}

impl Engine {
    /// Updates a trigger at the same boundary as the authoritative resting order, keying a new one's
    /// market at the least fill rate in force first.
    pub(crate) fn index_order(&mut self, account: AccountId, order: &RestingOrder, insert: bool) {
        if insert {
            self.key_triggers(order.market);
        }
        if let Some(indexes) = self.hot_indexes.as_mut() {
            indexes.order(account, order, insert);
        }
    }

    /// Re-keys `market`'s resting limits when the least fill rate in force is not the one they are
    /// keyed at: its bands or its close-spread ceiling moved, or the protocol version did.
    pub(crate) fn key_triggers(&mut self, market: MarketId) {
        let rate = self.least_fill_rate(market);
        let Some(indexes) = self.hot_indexes.as_mut() else { return };
        let accounts = &self.accounts;
        let orders = self.markets[&market].resting.iter().map(|(order_id, account)| {
            // The resting book and the accounts' orders change together (`Engine::remove_order`).
            (*account, accounts[account].orders.get(order_id).expect("a resting book entry is its account's order"))
        });
        indexes.key_triggers(market, rate, orders);
    }
}

/// The index mid at which a limit at `price` first comes within reach of a fill paying at least `rate`
/// tenths of a bp of the mid, floored to the raw unit: a buy's highest mid `m` with
/// `m + floor(m x rate) <= price`, and a sell's lowest with `m - floor(m x rate) >= price`, a rate of
/// 100% or more taken as just under it. A rate of zero keys the limit at its price, as does a product
/// past the `i128` range, which only widens what a price visits.
fn reach(price: Fixed, side: Side, rate: i64) -> Fixed {
    let (limit, rate) = (price.raw(), i128::from(rate).clamp(0, WHOLE_TENTH_BPS - 1));
    let key = match side {
        Side::Buy => limit
            .checked_add(1)
            .and_then(|above| above.checked_mul(WHOLE_TENTH_BPS))
            .map(|scaled| (scaled - 1) / (WHOLE_TENTH_BPS + rate)),
        Side::Sell => (limit - 1).checked_mul(WHOLE_TENTH_BPS).map(|scaled| scaled / (WHOLE_TENTH_BPS - rate) + 1),
    };
    key.map_or(price, Fixed::from_raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a fill at `mid` costs at least, in raw units, at `rate` tenths of a bp.
    fn least_spread(mid: i128, rate: i128) -> i128 {
        mid * rate / WHOLE_TENTH_BPS
    }

    #[test]
    fn a_limit_is_keyed_at_the_last_mid_a_fill_paying_the_rate_could_reach() {
        for limit in (1..=3_000).chain([999_999_999_937, 100_000 * 1_000_000_000 + 7]) {
            for rate in [0, 1, 7, 10, 99, 500, 1_234, 50_000, 99_999] {
                let price = Fixed::from_raw(limit);
                // A buy fills only where the mid plus the least spread is at or under the limit.
                let buy = reach(price, Side::Buy, rate).raw();
                assert!(buy + least_spread(buy, rate.into()) <= limit, "buy {limit} at {rate}: key {buy}");
                assert!(buy + 1 + least_spread(buy + 1, rate.into()) > limit, "buy {limit} at {rate}: key {buy}");
                // A sell fills only where the mid less the least spread is at or over the limit.
                let sell = reach(price, Side::Sell, rate).raw();
                assert!(sell - least_spread(sell, rate.into()) >= limit, "sell {limit} at {rate}: key {sell}");
                assert!(sell - 1 - least_spread(sell - 1, rate.into()) < limit, "sell {limit} at {rate}: key {sell}");
            }
        }
    }

    #[test]
    fn a_zero_rate_or_an_overflowing_product_keys_a_limit_at_its_price() {
        for price in [Fixed::from_raw(1), Fixed::from_int(100_000), Fixed::from_raw(i128::MAX / 10)] {
            for side in [Side::Buy, Side::Sell] {
                assert_eq!(reach(price, side, 0), price);
            }
        }
        let huge = Fixed::from_raw(i128::MAX / 10);
        assert_eq!(reach(huge, Side::Buy, 10), huge);
        assert_eq!(reach(huge, Side::Sell, 10), huge);
        // A rate of 100% or more is taken as just under it, so a sell still has a key.
        let price = Fixed::from_int(100);
        assert_eq!(reach(price, Side::Sell, 100_000), reach(price, Side::Sell, 99_999));
        assert_eq!(reach(price, Side::Buy, 250_000), reach(price, Side::Buy, 99_999));
    }
}
