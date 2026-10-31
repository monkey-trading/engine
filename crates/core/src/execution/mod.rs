//! Orders, pricing, and fills.
//!
//! Every fill is against the house at the live index mid plus a spread made of three parts:
//! - the stress level's minimum spread (zero in normal conditions);
//! - the account's toxicity add-on (zero for normal flow);
//! - the size spread, charged only on the slice of the trade that grows the account's absolute
//!   position through the market's size bands.
//!
//! Normal trades therefore fill at the mid. A market quoted in another currency than USD fills only
//! while its FX market has a price, and its realised PnL converts to USD at that FX index.

use std::cell::OnceCell;

use crate::Fixed;
use crate::access::AccessEvidence;
use crate::account::{Account, RestingOrder};
use crate::auth::RequestAuth;
use crate::command::{OrderKind, OrderRequest};
use crate::eip712::{cancel_hash, order_hash};
use crate::engine::{Engine, Markout};
use crate::event::{CancelReason, Event, Fill, Reject};
use crate::fixed::Rounding;
use crate::market::staircase;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId, OrderId, Side};

use self::validation::{growth, profit_fee};

/// What the live price is checked against before filling.
#[derive(Clone, Copy)]
pub(crate) enum PriceGuard {
    None,
    /// Market order: decline if the live price is more than `max_bps` worse than the price the
    /// client saw.
    Quote {
        quoted: Fixed,
        max_bps: i64,
    },
    /// Triggered limit: never fill worse than the limit.
    Limit(Fixed),
}

/// The house totals a risk check reads. The NAV is taken at once, and the gross exposure on first
/// use, so a run of checks over one state computes each once.
struct HouseTotals {
    nav: Fixed,
    gross: OnceCell<Fixed>,
}

impl HouseTotals {
    fn of(engine: &Engine) -> Self {
        Self { nav: engine.house_nav(), gross: OnceCell::new() }
    }

    fn gross(&self, engine: &Engine) -> Fixed {
        *self.gross.get_or_init(|| engine.house_gross_exposure())
    }
}

/// One fill to price, risk-check, and book.
pub(crate) struct Exec {
    pub account: AccountId,
    pub order_id: OrderId,
    pub market: MarketId,
    pub side: Side,
    pub qty: Fixed,
    pub reduce_only: bool,
    pub guard: PriceGuard,
    pub liquidation: bool,
}

/// A priced fill that has passed its price guard but is not yet booked.
struct Priced {
    qty: Fixed,
    delta: Fixed,
    price: Fixed,
    mid: Fixed,
    /// The FX index the fill converts to USD at; `None` for a USD market.
    fx: Option<Fixed>,
    spread_bps: i64,
    increases: bool,
}

impl Engine {
    /// Authorises the request, then fills a market order now or rests a limit or stop order,
    /// emitting acceptance or rejection. A resting order is checked against the live price at once:
    /// from `V14` only that order, and before `V14` the market's whole book.
    pub(crate) fn place_order(
        &mut self,
        req: &OrderRequest,
        evidence: Option<&AccessEvidence>,
        events: &mut Vec<Event>,
    ) {
        let reject = |reason, events: &mut Vec<Event>| {
            events.push(Event::OrderRejected { account: req.account, client_id: req.client_id, reason })
        };
        let authorised = self.authorise(req.account, req.auth.as_ref(), |domain, auth| {
            order_hash(domain, req, auth.nonce, auth.expires_at)
        });
        let request_hash = self
            .access_domain()
            .zip(req.auth.as_ref())
            .map(|(domain, auth)| order_hash(&domain, req, auth.nonce, auth.expires_at))
            .unwrap_or_default();
        if let Err(reason) = authorised
            .and_then(|()| {
                self.check_access(Some(req.account), None, request_hash, evidence, false, true)
                    .map_err(|reason| Reject::Access { reason })
            })
            .and_then(|()| self.validate_order(req))
        {
            return reject(reason, events);
        }
        self.refresh_session(req.market, events);

        match req.kind {
            OrderKind::Market { quoted, max_slippage_bps } => {
                let house_bound = self.markets[&req.market].spec.max_slippage_bps;
                // The client may tighten the house bound but not widen it.
                let max_bps = max_slippage_bps.map_or(house_bound, |client| client.clamp(0, house_bound));
                let order_id = self.alloc_order_id();
                let exec = Exec {
                    account: req.account,
                    order_id,
                    market: req.market,
                    side: req.side,
                    qty: req.qty,
                    reduce_only: req.reduce_only,
                    guard: PriceGuard::Quote { quoted, max_bps },
                    liquidation: false,
                };
                match self.execute(exec, events) {
                    Ok(fill) => {
                        events.push(Event::OrderAccepted { account: req.account, client_id: req.client_id, order_id });
                        self.after_fill(fill, events);
                    }
                    Err(reason) => reject(reason, events),
                }
            }
            OrderKind::Limit { .. } | OrderKind::Stop { .. } => {
                let order_id = self.alloc_order_id();
                let order = RestingOrder {
                    order_id,
                    client_id: req.client_id,
                    market: req.market,
                    side: req.side,
                    qty: req.qty,
                    kind: req.kind,
                    reduce_only: req.reduce_only,
                };
                self.index_order(req.account, &order, true);
                self.accounts.get_mut(&req.account).expect("validated").orders.insert(order_id, order);
                self.markets.get_mut(&req.market).expect("validated").resting.insert(order_id, req.account);
                self.record_order(req.account, order_id);
                events.push(Event::OrderAccepted { account: req.account, client_id: req.client_id, order_id });
                if self.protocol_version < ProtocolVersion::V14 {
                    self.trigger_resting(req.market, events);
                } else if let Some(mid) = self.trigger_mid(req.market) {
                    self.trigger_order(order_id, req.account, mid, events);
                }
            }
        }
    }

    /// Authorises the request, then cancels a resting order on the client's request.
    pub(crate) fn cancel_order(
        &mut self,
        account: AccountId,
        order_id: OrderId,
        auth: Option<&RequestAuth>,
        events: &mut Vec<Event>,
    ) {
        if let Err(reason) = self.authorise(account, auth, |domain, auth| {
            cancel_hash(domain, account, order_id, auth.nonce, auth.expires_at)
        }) {
            events.push(Event::OrderRejected { account, client_id: 0, reason });
        } else if self.remove_order(account, order_id).is_some() {
            events.push(Event::OrderCancelled { account, order_id, reason: CancelReason::Client });
        } else {
            events.push(Event::OrderRejected { account, client_id: 0, reason: Reject::UnknownOrder });
        }
    }

    /// Fills resting orders whose trigger the live price has reached. While a market is closed, or
    /// its FX market has no price, nothing triggers, so the first fresh price after that fills
    /// everything that gapped through, at that price.
    pub(crate) fn trigger_resting(&mut self, market_id: MarketId, events: &mut Vec<Event>) {
        let Some(mid) = self.trigger_mid(market_id) else { return };
        self.ensure_hot_indexes();
        let resting = self.hot_indexes.as_ref().expect("initialized").triggered(market_id, mid);
        for (order_id, account) in resting {
            self.trigger_order(order_id, account, mid, events);
        }
    }

    /// Prices and risk-checks one fill, then books it. From `V13` the market's stress level is
    /// re-evaluated first, so the fill pays the level its inputs set now, after any fill before it;
    /// nothing else is mutated on error.
    /// Error cases:
    /// - the market is closed, or has no valid index, or its FX market has no price
    /// - reduce-only with no opposite position, or new risk in a reduce-only market
    /// - new risk below the market's minimum order notional
    /// - a spread at this size that reaches the price itself
    /// - a live price beyond the slippage bound or the limit
    /// - not enough margin, or no house capacity, for the risk the fill adds
    pub(crate) fn execute(&mut self, exec: Exec, events: &mut Vec<Event>) -> Result<Fill, Reject> {
        if self.protocol_version >= ProtocolVersion::V13 {
            let nav = self.house_nav();
            self.refresh_stress(exec.market, nav, events);
        }
        let priced = self.price(&exec)?;
        if priced.increases && !exec.liquidation {
            self.check_risk(&exec, &priced, &HouseTotals::of(self))?;
        }
        Ok(self.book(&exec, priced))
    }

    /// Records a fill and writes off any balance the account can no longer cover.
    pub(crate) fn after_fill(&mut self, fill: Fill, events: &mut Vec<Event>) {
        let account = fill.account;
        events.push(Event::Fill(fill));
        self.settle_bad_debt(account, events);
    }

    /// The next order id; ids are never reused.
    pub(crate) fn alloc_order_id(&mut self) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        id
    }

    /// The live mid resting orders in `market_id` trigger against, or `None` while the market is
    /// closed, has no valid index, or its FX market has no price.
    fn trigger_mid(&self, market_id: MarketId) -> Option<Fixed> {
        let market = &self.markets[&market_id];
        if !market.open || self.live_fx(market_id).is_err() {
            return None;
        }
        market.index(self.now).ok().map(|index| index.mid)
    }

    /// Fills one resting order if `mid` has reached its trigger. A limit the spread keeps out of
    /// reach stays resting; any other failure cancels it, and from `V39` a reversal the market's
    /// close-spread ceiling refuses fills its closing slice before the cancel.
    fn trigger_order(&mut self, order_id: OrderId, account: AccountId, mid: Fixed, events: &mut Vec<Event>) {
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
        };
        match self.execute(exec, events) {
            Ok(fill) => {
                self.remove_order(account, order_id);
                self.after_fill(fill, events);
            }
            Err(Reject::Slippage { .. }) => {}
            Err(Reject::CloseSpreadCeiling { .. }) => self.fill_closing_slice(account, order_id, &order, guard, events),
            Err(_) => {
                self.remove_order(account, order_id);
                events.push(Event::OrderCancelled { account, order_id, reason: CancelReason::Unfillable });
            }
        }
    }

    /// The live price for a fill and the slice of it that adds risk. Notional bounds are in USD; the
    /// conversions round so the minimum and the size spread are never less than exact.
    /// Error cases:
    /// - the market is closed, or has no valid index, or its FX market has no price
    /// - reduce-only with no opposite position, or new risk in a reduce-only market
    /// - new risk below the market's minimum order notional
    /// - a spread at this size that reaches the price itself
    /// - a live price beyond the slippage bound or the limit
    fn price(&self, exec: &Exec) -> Result<Priced, Reject> {
        let params = self.params.as_ref().expect("callers check params are set");
        let market = &self.markets[&exec.market];
        if !market.open {
            return Err(Reject::MarketClosed);
        }
        let mid = market.index(self.now).map_err(|error| Reject::NoPrice { error })?.mid;
        let fx = self.live_fx(exec.market)?;
        let currency = market.spec.quote_currency;
        let usd = |amount, rounding| currency.to_usd(amount, fx, rounding);
        let acct = &self.accounts[&exec.account];
        let pos = acct.position(exec.market);

        let qty = if exec.reduce_only {
            if pos.qty.is_zero() || pos.qty.signum() == exec.side.sign() {
                return Err(Reject::ReduceOnly);
            }
            exec.qty.min(pos.qty.abs())
        } else {
            exec.qty
        };
        let delta = exec.side.signed(qty);
        let (grow_from, grow_to) = growth(pos.qty, delta);
        let increases = grow_to > grow_from;
        if increases && self.is_reduce_only(market) {
            return Err(Reject::ReduceOnly);
        }
        if increases && usd(qty.mul(mid), Rounding::Down) < market.spec.min_order_notional {
            return Err(Reject::InvalidOrder { reason: "below minimum order notional".into() });
        }

        let stress = &params.stress_levels[market.stress.level];
        let toxicity_bps = if exec.liquidation { 0 } else { params.toxicity_spread_bps(acct.toxicity_bps) };
        // The spread per unit as its parts, every charge rounded as `charge` says: the stress and
        // toxicity charge over the whole quantity, and the size cost per unit, charged only on the
        // slice that grows the position. Widening the growth slice outwards can only add to its
        // cost, and dividing the size cost by qty (not by notional) keeps full precision.
        let parts = |charge: Rounding| {
            let (from, to) =
                (usd(grow_from.mul(mid), Rounding::Down), usd(grow_to.mul_rounded(mid, charge), Rounding::Up));
            let size_cost_usd = staircase(&market.spec.spread_tiers, from, to, charge)
                .mul_bps_rounded(stress.size_spread_multiplier_bps, charge);
            let size_cost = currency.from_usd(size_cost_usd, fx, Rounding::Up);
            let base = mid.mul_bps_rounded(stress.min_spread_bps + toxicity_bps, charge);
            (base, size_cost.div_rounded(qty, charge))
        };
        let spread = |charge: Rounding| {
            let (base, size) = parts(charge);
            base + size
        };
        // The truncated spread decides whether a spread reaches the price. From `V10` the spread rounds
        // up, and a sell's stops a unit short of the price, so rounding never makes it reach the price.
        // From `V39`, a reversal over its ceiling is classified first: its refusal fills a triggered
        // order's closing slice, which stays executable when the blended spread reaches the price.
        let closes = !pos.qty.is_zero() && (exec.side == Side::Buy) != pos.qty.is_positive();
        let over_ceiling = self.protocol_version >= ProtocolVersion::V39
            && closes
            && increases
            && self
                .close_spread_ceiling(exec.market)
                .is_some_and(|ceiling| spread(Rounding::TowardZero).raw() * 10_000 > mid.raw() * i128::from(ceiling));
        if !over_ceiling && spread(Rounding::TowardZero) >= mid {
            return Err(Reject::InvalidOrder { reason: "spread at this size reaches the price".into() });
        }
        let charge = self.charge_rounding();
        let (base, size) = parts(charge);
        // From `V39`, a close's spread cannot exceed its market's ceiling, and a reversal over it
        // is refused.
        let rounded = self.bounded_close_charge(exec.market, exec.side, pos.qty, increases, base + size, mid)?;
        let adjust = if exec.side == Side::Sell { rounded.min(mid - Fixed::from_raw(1)) } else { rounded };
        let price = mid + adjust.mul_int(exec.side.sign());

        match exec.guard {
            PriceGuard::None => {}
            PriceGuard::Quote { quoted, max_bps } => {
                let adverse = (price - quoted).mul_int(exec.side.sign());
                if adverse > quoted.mul_bps(max_bps) {
                    return Err(Reject::Slippage { quoted, live: price, slippage_bps: adverse.bps_of(quoted) });
                }
            }
            PriceGuard::Limit(limit) => {
                if (price - limit).mul_int(exec.side.sign()).is_positive() {
                    return Err(Reject::Slippage { quoted: limit, live: price, slippage_bps: 0 });
                }
            }
        }
        Ok(Priced { qty, delta, price, mid, fx, spread_bps: adjust.bps_of(mid), increases })
    }

    /// Checks a fill that adds risk against the account's margin and the house's exposure caps, in
    /// USD: realised PnL converts rounded down, and exposure rounded up.
    /// Error cases:
    /// - the account's equity after the fill would be below its margin requirement
    /// - the fill would push the house's net exposure past the market cap or the total cap
    fn check_risk(&self, exec: &Exec, priced: &Priced, house: &HouseTotals) -> Result<(), Reject> {
        let params = self.params.as_ref().expect("callers check params are set");
        let market = &self.markets[&exec.market];
        let usd = |amount, rounding| market.spec.quote_currency.to_usd(amount, priced.fx, rounding);

        // Margin reads only cash and positions, so the resting orders are not copied.
        let acct = &self.accounts[&exec.account];
        let mut sim = Account { balance: acct.balance, positions: acct.positions.clone(), ..Account::default() };
        let version = self.protocol_version;
        let realised = sim.positions.entry(exec.market).or_default().apply(priced.delta, priced.price, version);
        let realised = usd(realised, Rounding::Down);
        sim.balance += realised - profit_fee(realised, params.profit_fee_bps, version);
        let (equity, required) = self.equity_and_margin(&sim, Some((exec.market, priced.mid, priced.fx)));
        if equity < required {
            return Err(Reject::InsufficientMargin { equity, required });
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

    /// Books a priced fill: the position, the market aggregates, realised PnL in USD (rounded down)
    /// and the profit fee against house cash, the PnL in the account's emission period result, and a
    /// markout to score the flow later.
    fn book(&mut self, exec: &Exec, priced: Priced) -> Fill {
        let params = self.params.as_ref().expect("callers check params are set");
        let fee_bps = params.profit_fee_bps;
        let version = self.protocol_version;
        let markout_due = self.now + params.toxicity.horizon_ms;
        let market = self.markets.get_mut(&exec.market).expect("priced against this market");
        let acct = self.accounts.get_mut(&exec.account).expect("priced for this account");

        let pos = acct.positions.entry(exec.market).or_default();
        market.client_qty -= pos.qty;
        market.client_cost -= pos.cost();
        let realised = pos.apply(priced.delta, priced.price, version);
        let realised_pnl = market.spec.quote_currency.to_usd(realised, priced.fx, Rounding::Down);
        market.client_qty += pos.qty;
        market.client_cost += pos.cost();
        let position = pos.qty;
        if position.is_zero() {
            acct.positions.remove(&exec.market);
        }
        let fee = profit_fee(realised_pnl, fee_bps, version);
        acct.balance += realised_pnl - fee;
        self.house_cash -= realised_pnl - fee;

        self.record_position(exec.account, exec.market);
        self.record_period_result(exec.account, realised_pnl, Fixed::ZERO);
        if !exec.liquidation {
            let quote = self.markets[&exec.market].spec.quote_currency;
            let notional = quote.to_usd(priced.qty.mul(priced.price), priced.fx, Rounding::Down);
            let (account, market, side, price) = (exec.account, exec.market, exec.side, priced.price);
            self.record_markout(Markout { due: markout_due, account, market, side, price, notional });
        }
        Fill {
            account: exec.account,
            order_id: exec.order_id,
            market: exec.market,
            side: exec.side,
            qty: priced.qty,
            price: priced.price,
            index: priced.mid,
            spread_bps: priced.spread_bps,
            realised_pnl,
            fx_index: priced.fx,
            profit_fee: fee,
            position,
            liquidation: exec.liquidation,
        }
    }

    /// How a spread charge or a margin requirement rounds: up from `V10`, so a trader never pays or
    /// posts less than the exact amount, and toward zero before.
    pub(crate) fn charge_rounding(&self) -> Rounding {
        if self.protocol_version >= ProtocolVersion::V10 { Rounding::Up } else { Rounding::TowardZero }
    }

    /// Removes a resting order from its account and its market's book.
    pub(crate) fn remove_order(&mut self, account: AccountId, order_id: OrderId) -> Option<RestingOrder> {
        let order = self.accounts.get_mut(&account)?.orders.remove(&order_id)?;
        self.index_order(account, &order, false);
        if let Some(market) = self.markets.get_mut(&order.market) {
            market.resting.remove(&order_id);
        }
        self.record_order(account, order_id);
        Some(order)
    }
}

mod quote;
pub use quote::{Quote, Quoter};
mod validation;

#[cfg(test)]
mod tests;
