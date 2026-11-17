//! Splits preflight every price, quantity and cash transfer before committing any mutation.

use std::collections::BTreeSet;

use crate::Fixed;
use crate::account::Position;
use crate::command::OrderKind;
use crate::corporate::CorporateAction;
use crate::corporate::cash::{usd_down, valuation_fx};
use crate::engine::Engine;
use crate::event::{CancelReason, Event};
use crate::fixed::Rounding;
use crate::levels::{LevelsRemoved, PositionLevels};
use crate::types::{AccountId, OrderId, Side};

/// Why a split fails when a scaled position quantity leaves the input bound.
const BOUND: &str = "split quantity exceeds the input bound";
/// Why a split fails when a scaled value leaves the representable range.
const OVERFLOW: &str = "split quantity or price overflows";

/// One account's changes from a split, computed before any is applied.
struct ScaledAccount {
    account: AccountId,
    /// The position's scaled books, `None` when it was rounded away; unset when the account held none.
    position: Option<Option<Position>>,
    /// The balance after the cash in lieu, when one was settled.
    balance: Option<Fixed>,
    /// Signed USD realised on the fraction rounded off the position, when one was rounded off.
    in_lieu: Option<Fixed>,
    /// Each resting order in the market, with its scaled kind, quantity and from `V79` levels, or `None`
    /// to cancel it.
    orders: Vec<(OrderId, Option<ScaledOrder>)>,
    /// From `V79`, the position's scaled levels, `None` when the split took them off; unset when it held
    /// none.
    levels: Option<Option<PositionLevels>>,
    /// From `V79`, the levels the split took off the position: rounded to zero, or a level scaled out of
    /// range.
    dropped: Option<PositionLevels>,
}

/// A resting order's scaled kind and quantity, and from `V79` its scaled levels when it names any.
type ScaledOrder = (OrderKind, Fixed, Option<PositionLevels>);

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
            let state = &self.accounts[&account];
            let (mut position_change, mut balance, mut in_lieu) = (None, None, None);
            let (mut levels_change, mut dropped) = (None, None);
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
                    balance = Some(Fixed::from_raw(state.balance.raw().checked_add(amount.raw()).ok_or(OVERFLOW)?));
                    house = house.checked_sub(amount.raw()).ok_or(OVERFLOW)?;
                    in_lieu = Some(amount);
                }
                let rounded = Position { qty, entry };
                market.client_qty = market.client_qty - position.qty + qty;
                market.client_cost = market.client_cost - position.cost() + rounded.cost();
                position_change = Some((!qty.is_zero()).then_some(rounded));
                // From `V79` the position's levels scale as its resting orders' prices do, and go with
                // a position rounded away or a level scaled out of range.
                if let Some(levels) = state.levels.get(&action.market).copied() {
                    let side = if position.qty.is_positive() { Side::Buy } else { Side::Sell };
                    let kept = (!qty.is_zero()).then(|| scale_levels(levels, side, old, new)).flatten();
                    dropped = kept.is_none().then_some(levels);
                    levels_change = Some(kept);
                }
            }
            let mut orders = Vec::new();
            for order in state.orders.values().filter(|o| o.market == action.market) {
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
                let qty = scale_quantity(order.qty, new, old)?.0;
                // From `V79` an entry order's levels scale as its position's would.
                let named = PositionLevels::named(order.take_profit, order.stop_loss);
                let levels = named.map(|levels| scale_levels(levels, order.side, old, new));
                match (scaled, levels) {
                    (Some((kind, price)), None | Some(Some(_)))
                        if price.is_positive() && !qty.is_zero() && qty <= Fixed::MAX_INPUT =>
                    {
                        orders.push((order.order_id, Some((kind, qty, levels.flatten()))));
                    }
                    _ => {
                        market.resting.remove(&order.order_id);
                        orders.push((order.order_id, None));
                    }
                }
            }
            let (position, levels) = (position_change, levels_change);
            scaled.push(ScaledAccount { account, position, balance, in_lieu, orders, levels, dropped });
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
        for ScaledAccount { account, position, balance, in_lieu, orders, levels, dropped } in scaled {
            let state = self.accounts.get_mut(&account).expect("a scaled account exists");
            let held = state.positions.contains_key(&action.market);
            match position {
                Some(Some(rounded)) => {
                    state.positions.insert(action.market, rounded);
                }
                Some(None) => {
                    state.positions.remove(&action.market);
                    state.opened_at.remove(&action.market);
                }
                None => {}
            }
            if let Some(balance) = balance {
                state.balance = balance;
            }
            match levels {
                Some(Some(scaled)) => {
                    state.levels.insert(action.market, scaled);
                }
                Some(None) => {
                    state.levels.remove(&action.market);
                }
                None => {}
            }
            let mut cancelled = Vec::new();
            for (order_id, change) in orders {
                match change {
                    Some((kind, qty, levels)) => {
                        let order = state.orders.get_mut(&order_id).expect("a scaled order rests");
                        (order.kind, order.qty) = (kind, qty);
                        if let Some(levels) = levels {
                            (order.take_profit, order.stop_loss) = (levels.take_profit, levels.stop_loss);
                        }
                    }
                    None => {
                        state.orders.remove(&order_id);
                        cancelled.push(order_id);
                    }
                }
            }
            let resting: Vec<_> =
                state.orders.values().filter(|o| o.market == action.market).map(|o| o.order_id).collect();
            self.scale_volume_lots(account, action.market, new, old);
            if held {
                self.record_position(account, action.market);
            }
            if levels.is_some() {
                // The split moved every price in the market, so each level is keyed again.
                self.record_levels(account, action.market);
            }
            for order_id in resting.into_iter().chain(cancelled.iter().copied()) {
                self.record_order(account, order_id);
            }
            if let Some(amount) = in_lieu {
                // Realised P&L on the position, like a fill's, so it counts toward emission weight.
                self.record_period_result(account, amount, Fixed::ZERO);
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
            if let Some(PositionLevels { take_profit, stop_loss }) = dropped {
                let (market, reason) = (action.market, LevelsRemoved::Split);
                events.push(Event::PositionLevelsRemoved { account, market, take_profit, stop_loss, reason });
            }
            if held && self.accounts[&account].positions.is_empty() {
                // Removing the last position settles any debt the account was waiting to write off.
                self.settle_bad_debt(account, events);
            }
        }
        // From `V54` a markout is measured from its fill's index, and matches no fill after a split.
        for (offset, price) in markouts {
            self.markouts[offset].price = price;
            (self.markouts[offset].open, self.markouts[offset].closed) = (Fixed::ZERO, Fixed::ZERO);
            self.record_markout_change(offset);
        }
        // Only the split market's derived indexes depend on what changed: its holders follow
        // `record_position`, and its triggers and markout legs are rebuilt here.
        self.forget_markout_legs(action.market);
        self.rekey_triggers(action.market);
        Ok(())
    }
}

/// A position's levels on `side` (long for a buy) scaled by `new / old` as resting orders' prices are:
/// its take profit as a limit closing it, its stop loss as a stop, so neither fires sooner; `None` when
/// either rounds to zero or leaves the input bound.
fn scale_levels(levels: PositionLevels, side: Side, old: u32, new: u32) -> Option<PositionLevels> {
    // The take profit closes a long as a sell limit, rounded up, and the stop loss as a sell stop,
    // rounded down; a short's the other way.
    let (take, stop) = match side {
        Side::Buy => (Rounding::Up, Rounding::Down),
        Side::Sell => (Rounding::Down, Rounding::Up),
    };
    let scaled = |level: Option<Fixed>, rounding| match level {
        None => Some(None),
        Some(level) => scale(level, old, new, rounding).ok().filter(|price| price.is_valid_input()).map(Some),
    };
    Some(PositionLevels { take_profit: scaled(levels.take_profit, take)?, stop_loss: scaled(levels.stop_loss, stop)? })
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
