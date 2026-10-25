//! Splits preflight every price, quantity and cash transfer before committing any mutation.

use std::collections::BTreeSet;

use crate::Fixed;
use crate::account::{Account, Position};
use crate::command::OrderKind;
use crate::corporate::CorporateAction;
use crate::corporate::cash::{usd_down, valuation_fx};
use crate::engine::Engine;
use crate::event::{CancelReason, Event};
use crate::fixed::Rounding;
use crate::types::{AccountId, OrderId, Side};

/// Why a split fails when a scaled position quantity leaves the input bound.
const BOUND: &str = "split quantity exceeds the input bound";
/// Why a split fails when a scaled value leaves the representable range.
const OVERFLOW: &str = "split quantity or price overflows";

/// One account's books after a split, with what the split settles and cancels for it.
struct ScaledAccount {
    account: AccountId,
    state: Account,
    /// Signed USD realised on the fraction rounded off the position, when one was rounded off.
    in_lieu: Option<Fixed>,
    cancelled: Vec<OrderId>,
}

impl Engine {
    /// Scales all current-unit references atomically while retaining cash and historical notionals.
    /// Each scaled price rounds once: an entry against its holder, a resting limit or stop price so
    /// it never executes sooner, and market prices toward zero. Quantities round toward zero; the
    /// fraction rounded off a position is closed at the post-split mark, and a resting order left
    /// with no quantity or price, above the input bound, or with an unrepresentable price, is
    /// cancelled. An account the split
    /// leaves flat settles its bad debt. Same-open dividends have already paid, and later ones are
    /// per share held at their own previous close, so no pending entitlement changes units.
    /// Error cases:
    /// - a market price or entry rounds to zero, or a scaled value overflows
    /// - a position quantity would exceed the input bound
    /// - a rounded-off fraction has no post-split mark or usable FX valuation to settle at
    pub(super) fn apply_corporate_split(
        &mut self,
        action: &CorporateAction,
        new: u32,
        old: u32,
        events: &mut Vec<Event>,
    ) -> Result<(), String> {
        let market_price = |value| positive_price(value, old, new, Rounding::TowardZero);
        let mut market = self.markets[&action.market].clone();
        market.mark = market.mark.map(market_price).transpose()?;
        market.processed_index = market
            .processed_index
            .map(|mut index| {
                index.mid = market_price(index.mid)?;
                Ok::<_, String>(index)
            })
            .transpose()?;
        for quote in market.quotes.values_mut() {
            quote.mid = market_price(quote.mid)?;
        }
        market.stress.move_ref =
            market.stress.move_ref.map(|(at, value)| market_price(value).map(|value| (at, value))).transpose()?;
        for bucket in &mut market.stress.move_window {
            bucket.low = market_price(bucket.low)?;
            bucket.high = market_price(bucket.high)?;
        }
        market.last_split_at = Some(action.ex_open);
        let fx = valuation_fx(&market);
        let holders = self.hot_indexes.as_ref().expect("apply builds derived indexes").holders.get(&action.market);
        let affected: BTreeSet<_> = holders
            .into_iter()
            .flat_map(|holders| holders.iter().copied())
            .chain(market.resting.values().copied())
            .collect();
        let mut house = self.house_cash.raw();
        let mut scaled = Vec::with_capacity(affected.len());
        for account in affected {
            let mut state = self.accounts[&account].clone();
            let mut in_lieu = None;
            if let Some(position) = state.positions.get(&action.market).copied() {
                let (qty, remainder) = scale_quantity(position.qty, new, old)?;
                if qty.abs() > Fixed::MAX_INPUT {
                    return Err(BOUND.into());
                }
                let against_holder = if position.qty.is_positive() { Rounding::Up } else { Rounding::Down };
                let entry = positive_price(position.entry, old, new, against_holder)?;
                if remainder != 0 {
                    let mark = market.mark.ok_or("cash in lieu needs a post-split mark")?;
                    // Closing `remainder / old` raw units at the mark realises its PnL, rounded down.
                    let pnl = (mark - entry)
                        .checked_mul_div(Fixed::from_raw(remainder), Fixed::from_int(old.into()), Rounding::Down)
                        .ok_or(OVERFLOW)?;
                    let amount = usd_down(market.spec.quote_currency, pnl, fx.clone()?)?;
                    state.balance = Fixed::from_raw(state.balance.raw().checked_add(amount.raw()).ok_or(OVERFLOW)?);
                    house = house.checked_sub(amount.raw()).ok_or(OVERFLOW)?;
                    in_lieu = Some(amount);
                }
                let rounded = Position { qty, entry };
                market.client_qty = market.client_qty - position.qty + qty;
                market.client_cost = market.client_cost - position.cost() + rounded.cost();
                if qty.is_zero() {
                    state.positions.remove(&action.market);
                } else {
                    state.positions.insert(action.market, rounded);
                }
            }
            let mut cancelled = Vec::new();
            for order in state.orders.values_mut().filter(|o| o.market == action.market) {
                // A client chooses its order's size and price, so an order that leaves the input
                // bound, rounds to zero or overflows is cancelled rather than failing the split.
                let (limit, stop) = match order.side {
                    Side::Buy => (Rounding::Down, Rounding::Up),
                    Side::Sell => (Rounding::Up, Rounding::Down),
                };
                let scaled = match order.kind {
                    OrderKind::Limit { price } => {
                        scale(price, old, new, limit).ok().map(|price| (OrderKind::Limit { price }, price))
                    }
                    OrderKind::Stop { price } => {
                        scale(price, old, new, stop).ok().map(|price| (OrderKind::Stop { price }, price))
                    }
                    OrderKind::Market { .. } => unreachable!("only limit and stop orders rest"),
                };
                // A resting quantity was within the input bound, so its product with a `u32` fits.
                order.qty = scale_quantity(order.qty, new, old)?.0;
                match scaled {
                    Some((kind, price))
                        if price.is_positive() && !order.qty.is_zero() && order.qty <= Fixed::MAX_INPUT =>
                    {
                        order.kind = kind;
                    }
                    _ => cancelled.push(order.order_id),
                }
            }
            for order_id in &cancelled {
                state.orders.remove(order_id);
                market.resting.remove(order_id);
            }
            scaled.push(ScaledAccount { account, state, in_lieu, cancelled });
        }
        let markouts = self
            .markouts
            .iter()
            .enumerate()
            .filter(|(_, m)| m.market == action.market)
            .map(|(offset, m)| market_price(m.price).map(|value| (offset, value)))
            .collect::<Result<Vec<_>, _>>()?;
        // All fallible arithmetic is above this boundary. Cash moves only by the cash in lieu, and
        // USD limits and historical fill notionals keep their values.
        self.markets.insert(action.market, market);
        self.house_cash = Fixed::from_raw(house);
        events.push(Event::CorporateSplit {
            id: action.id.clone(),
            market: action.market,
            new_units: new,
            old_units: old,
        });
        for ScaledAccount { account, state, in_lieu, cancelled } in scaled {
            let orders: Vec<_> =
                state.orders.values().filter(|o| o.market == action.market).map(|o| o.order_id).collect();
            let held = self.accounts[&account].positions.contains_key(&action.market);
            self.accounts.insert(account, state);
            if held {
                self.record_position(account, action.market);
            }
            for order_id in orders.into_iter().chain(cancelled.iter().copied()) {
                self.record_order(account, order_id);
            }
            if let Some(amount) = in_lieu {
                events.push(Event::CorporateCashInLieu {
                    id: action.id.clone(),
                    market: action.market,
                    account,
                    amount,
                });
            }
            events.extend(cancelled.into_iter().map(|order_id| Event::OrderCancelled {
                account,
                order_id,
                reason: CancelReason::Split,
            }));
            if held && self.accounts[&account].positions.is_empty() {
                // Removing the last position settles any debt the account was waiting to write off.
                self.settle_bad_debt(account, events);
            }
        }
        for (offset, price) in markouts {
            self.markouts[offset].price = price;
            self.record_markout_change(offset);
        }
        self.hot_indexes = None;
        self.ensure_hot_indexes();
        Ok(())
    }
}

/// `value × numerator / denominator`, rounded once as `rounding` says.
/// Error cases:
/// - the result is unrepresentable
fn scale(value: Fixed, numerator: u32, denominator: u32, rounding: Rounding) -> Result<Fixed, String> {
    value
        .checked_mul_div(Fixed::from_raw(numerator.into()), Fixed::from_raw(denominator.into()), rounding)
        .ok_or_else(|| OVERFLOW.into())
}

/// A scaled market price or entry, which must stay positive to keep valuing the books.
/// Error cases:
/// - the result is unrepresentable or rounds to zero
fn positive_price(value: Fixed, numerator: u32, denominator: u32, rounding: Rounding) -> Result<Fixed, String> {
    let price = scale(value, numerator, denominator, rounding)?;
    if price.is_positive() { Ok(price) } else { Err("split price rounds to zero".into()) }
}

/// A quantity scaled by `new / old` toward zero, and the signed raw remainder of `qty × new` over
/// `old` that the rounding drops.
/// Error cases:
/// - the product overflows
fn scale_quantity(qty: Fixed, new: u32, old: u32) -> Result<(Fixed, i128), String> {
    let product = qty.raw().checked_mul(new.into()).ok_or(OVERFLOW)?;
    Ok((Fixed::from_raw(product / i128::from(old)), product % i128::from(old)))
}
