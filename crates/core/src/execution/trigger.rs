//! Resting orders a price reaches: each market's triggered orders, filled or cancelled, and from `V40`
//! the closing slice of a triggered reversal its close-spread ceiling refuses.

use crate::Fixed;
use crate::account::RestingOrder;
use crate::command::OrderKind;
use crate::engine::Engine;
use crate::event::{CancelReason, Event, Reject};
use crate::execution::{Exec, PriceGuard};
use crate::levels::{FillCause, PositionLevels};
use crate::types::{AccountId, MarketId, OrderId, Side};

impl Engine {
    /// Fills resting orders whose trigger the live price has reached: a stop's price, and a limit's
    /// price or, from `V74`, the mid where the market's least fill rate first lets it fill. While a
    /// market is closed, or its FX market has no price, nothing triggers, so the first fresh price
    /// after that fills everything that gapped through, at that price.
    pub(crate) fn trigger_resting(&mut self, market_id: MarketId, events: &mut Vec<Event>) {
        let Some(mid) = self.trigger_mid(market_id) else { return };
        self.ensure_hot_indexes();
        self.key_triggers(market_id);
        let resting = self.hot_indexes.as_ref().expect("initialized").triggered(market_id, mid);
        for (order_id, account) in resting {
            self.trigger_order(order_id, account, mid, events);
        }
    }

    /// The live mid resting orders in `market_id` trigger against, or `None` while the market is
    /// closed, has no valid index, or its FX market has no price.
    pub(crate) fn trigger_mid(&self, market_id: MarketId) -> Option<Fixed> {
        let market = &self.markets[&market_id];
        if !market.open || self.live_fx(market_id).is_err() {
            return None;
        }
        market.index(self.now).ok().map(|index| index.mid)
    }

    /// Fills one resting order if `mid` has reached its trigger. A limit the spread keeps out of
    /// reach stays resting; any other failure cancels it, and from `V40` a reversal the market's
    /// close-spread ceiling refuses fills its closing slice before the cancel.
    pub(super) fn trigger_order(&mut self, order_id: OrderId, account: AccountId, mid: Fixed, events: &mut Vec<Event>) {
        let Some(order) = self.accounts.get(&account).and_then(|acct| acct.orders.get(&order_id)).cloned() else {
            return;
        };
        let (triggered, guard) = match (order.kind, order.side) {
            (OrderKind::Limit { price }, Side::Buy) => (mid <= price, PriceGuard::Limit(price)),
            (OrderKind::Limit { price }, Side::Sell) => (mid >= price, PriceGuard::Limit(price)),
            (OrderKind::Stop { price }, Side::Buy) => (mid >= price, PriceGuard::None),
            (OrderKind::Stop { price }, Side::Sell) => (mid <= price, PriceGuard::None),
            // `place_order` fills market orders immediately; they never rest.
            (OrderKind::Market { .. }, _) => unreachable!("market orders never rest"),
        };
        if !triggered {
            return;
        }
        let exec = Exec {
            account,
            order_id,
            market: order.market,
            side: order.side,
            qty: order.qty,
            reduce_only: order.reduce_only,
            guard,
            liquidation: false,
            close_checked: false,
        };
        match self.execute(exec, events) {
            Ok(fill) => {
                self.remove_order(account, order_id);
                self.after_fill(fill, FillCause::Order, events);
                // From `V79` the position the fill leaves on the order's side takes its levels, which
                // are checked against its closing price at once.
                let named = PositionLevels::named(order.take_profit, order.stop_loss);
                self.attach_levels(account, order.market, order.side, named, events);
                if named.is_some() && self.accounts[&account].levels.contains_key(&order.market) {
                    self.check_attached_levels(account, order.market, events);
                }
            }
            Err(Reject::Slippage { .. }) => {}
            Err(Reject::CloseSpreadCeiling { .. }) => self.fill_closing_slice(account, order_id, &order, guard, events),
            Err(_) => {
                self.remove_order(account, order_id);
                events.push(Event::OrderCancelled { account, order_id, reason: CancelReason::Unfillable });
            }
        }
    }

    /// Fills the closing slice of a triggered order a reversal refusal stopped, then cancels the
    /// order: a triggered order has no client at hand to send the reduce-only close the refusal
    /// asks for. The closing slice is a pure close, so it fills at its clamped charge; if even it
    /// cannot fill, the order cancels as unfillable and the position stays open.
    pub(crate) fn fill_closing_slice(
        &mut self,
        account: AccountId,
        order_id: OrderId,
        order: &RestingOrder,
        guard: PriceGuard,
        events: &mut Vec<Event>,
    ) {
        let pos_qty = self.accounts.get(&account).map(|acct| acct.position(order.market).qty.abs()).unwrap_or_default();
        let fill = if pos_qty.is_zero() {
            None
        } else {
            let exec = Exec {
                account,
                order_id,
                market: order.market,
                side: order.side,
                qty: pos_qty.min(order.qty),
                reduce_only: true,
                guard,
                liquidation: false,
                close_checked: false,
            };
            self.execute(exec, events).ok()
        };
        self.remove_order(account, order_id);
        if let Some(fill) = fill {
            self.after_fill(fill, FillCause::Order, events);
            // The closing slice leaves no position on the order's side for its levels.
            let named = PositionLevels::named(order.take_profit, order.stop_loss);
            self.attach_levels(account, order.market, order.side, named, events);
        }
        events.push(Event::OrderCancelled { account, order_id, reason: CancelReason::Unfillable });
    }
}
