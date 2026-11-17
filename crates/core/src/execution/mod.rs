//! Orders, pricing, and fills.
//!
//! Every fill is against the house at the live index mid plus a spread made of three parts:
//! - the stress level's minimum spread (zero in normal conditions);
//! - the account's toxicity add-on (zero for normal flow);
//! - the size spread, charged through the market's size bands on the slice of the trade that grows
//!   the account's absolute position, and from `V52` on the slice that closes it too, both at the
//!   stress level's multiplier; from `V59` the first band ends at the account's volume tier's share.
//!
//! Before `V52` normal trades fill at the mid; from `V52` every fill pays its bands' charge. A market
//! quoted in another currency than USD fills only while its FX market has a price, and its realised
//! PnL converts to USD at that FX index.

use crate::Fixed;
use crate::access::AccessEvidence;
use crate::account::{Position, RestingOrder};
use crate::auth::RequestScope;
use crate::command::{OrderKind, OrderRequest};
use crate::eip712::order_hash;
use crate::engine::Engine;
use crate::event::{Event, Fill, Reject};
use crate::fixed::Rounding;
use crate::holding_fee;
use crate::levels::FillCause;
use crate::protocol::ProtocolVersion;
use crate::toxicity::FillMark;
use crate::types::{AccountId, MarketId, OrderId, Side};
use crate::volume_tier::AccountBands;

use self::validation::{closing, growth, profit_fee};

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
pub(crate) struct Priced {
    qty: Fixed,
    pub(crate) delta: Fixed,
    pub(crate) price: Fixed,
    pub(crate) mid: Fixed,
    /// The FX index the fill converts to USD at; `None` for a USD market.
    pub(crate) fx: Option<Fixed>,
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
        let authorised = self.authorise(req.account, req.auth.as_ref(), RequestScope::Trade, |domain, auth| {
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
        self.place_validated(req, FillCause::Order, events);
    }

    /// Fills or rests an order that has passed authorisation, access screening and validation, its fill
    /// booked as `cause`. Emits acceptance or rejection, as `place_order` does after those checks. From
    /// `V79` a market order's levels are checked against its priced fill before it is booked, and the
    /// position the fill leaves on the order's side takes them.
    fn place_validated(&mut self, req: &OrderRequest, cause: FillCause, events: &mut Vec<Event>) {
        let reject = |reason, events: &mut Vec<Event>| {
            events.push(Event::OrderRejected { account: req.account, client_id: req.client_id, reason })
        };
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
                let checked = |engine: &Self, priced: &Priced, share| engine.check_order_levels(req, priced, share);
                match self.execute_checked(exec, events, checked) {
                    Ok(fill) => {
                        events.push(Event::OrderAccepted { account: req.account, client_id: req.client_id, order_id });
                        self.after_fill(fill, cause, events);
                        self.attach_levels(req.account, req.market, req.side, req.levels(), events);
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
                    take_profit: req.take_profit,
                    stop_loss: req.stop_loss,
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

    /// Prices and risk-checks one fill, then books it. From `V13` the market's stress level is
    /// re-evaluated first, so the fill pays the level its inputs set now, after any fill before it;
    /// nothing else is mutated on error.
    /// Error cases:
    /// - the market is closed, or has no valid index, or its FX market has no price
    /// - reduce-only with no opposite position, or new risk in a reduce-only market
    /// - new risk below the market's minimum order notional
    /// - a spread at this size that reaches the price itself
    /// - a live price beyond the slippage bound or the limit
    /// - not enough margin, from `V64` at the account's tier's margin floor, or no house capacity, for
    ///   the risk the fill adds
    pub(crate) fn execute(&mut self, exec: Exec, events: &mut Vec<Event>) -> Result<Fill, Reject> {
        self.execute_checked(exec, events, |_, _, _| Ok(()))
    }

    /// `execute`, with `check` run on the priced fill, at the account's best-price share, before anything
    /// is booked, so its refusal refuses the fill.
    /// Error cases:
    /// - as `execute`
    /// - `check`'s refusal
    pub(crate) fn execute_checked(
        &mut self,
        exec: Exec,
        events: &mut Vec<Event>,
        check: impl FnOnce(&Self, &Priced, Option<i64>) -> Result<(), Reject>,
    ) -> Result<Fill, Reject> {
        if self.protocol_version >= ProtocolVersion::V13 {
            let nav = self.house_nav();
            self.refresh_stress(exec.market, nav, events);
        }
        // The account's tier, read once: its best-price share prices the fill, and its margin floor
        // raises the initial requirement risk is checked against.
        let tier = self.tier_in_force(exec.account);
        let share = tier.map(|tier| tier.best_price_share_bps);
        let priced = self.price(&exec, share)?;
        if priced.increases && !exec.liquidation {
            self.check_risk(&exec, &priced, &HouseTotals::of(self), self.margin_floor_of(tier))?;
        }
        check(self, &priced, share)?;
        Ok(self.book(&exec, priced))
    }

    /// Prices a close-out's fill at index `mid`, converting to USD at `fx`, and books it: from `V60`
    /// a weekend close-out fills at its market's mark once the market has shut, where its own
    /// index no longer prices; from `V67` at the market's level or the one the house's loss sets now,
    /// whichever is higher.
    /// Error cases:
    /// - as `price_at`
    pub(crate) fn execute_at(&mut self, exec: Exec, mid: Fixed, fx: Option<Fixed>) -> Result<Fill, Reject> {
        // A close-out only reduces, so it passes no risk check.
        assert!(exec.liquidation && exec.reduce_only, "only a close-out fills at a price it is given");
        let level = self.close_out_stress_level(exec.market);
        let priced = self.price_at(&exec, mid, fx, level, self.best_price_share(exec.account))?;
        Ok(self.book(&exec, priced))
    }

    /// Records a fill, from `V79` removes the levels of a position it closed or turned (`levels`), and
    /// writes off any balance the account can no longer cover.
    pub(crate) fn after_fill(&mut self, fill: Fill, cause: FillCause, events: &mut Vec<Event>) {
        let (account, market, side, qty, position) = (fill.account, fill.market, fill.side, fill.qty, fill.position);
        events.push(Event::Fill(fill));
        self.settle_levels(account, market, position - side.signed(qty), position, cause, events);
        self.settle_bad_debt(account, events);
    }

    /// The next order id; ids are never reused.
    pub(crate) fn alloc_order_id(&mut self) -> OrderId {
        let id = self.next_order_id;
        self.next_order_id += 1;
        id
    }

    /// The live price for a fill and the slice of it that adds risk. Notional bounds are in USD; the
    /// conversions round so the minimum and the size spread are never less than exact.
    /// Error cases:
    /// - the market is closed, or has no valid index, or its FX market has no price
    /// - reduce-only with no opposite position, or new risk in a reduce-only market
    /// - new risk below the market's minimum order notional
    /// - a spread at this size that reaches the price itself
    /// - a live price beyond the slippage bound or the limit
    ///
    /// `share` is the account's best-price share (`Engine::best_price_share`).
    pub(crate) fn price(&self, exec: &Exec, share: Option<i64>) -> Result<Priced, Reject> {
        let market = &self.markets[&exec.market];
        if !market.open {
            return Err(Reject::MarketClosed);
        }
        let mid = market.index(self.now).map_err(|error| Reject::NoPrice { error })?.mid;
        let fx = self.live_fx(exec.market)?;
        self.price_at(exec, mid, fx, market.stress.level, share)
    }

    /// The price for a fill at index `mid`, converting to USD at `fx`, at stress level `level`, and the
    /// slice of it that adds risk, as `price` gives it at the live index.
    /// Error cases:
    /// - as `price`, but for the market's session and prices
    fn price_at(
        &self,
        exec: &Exec,
        mid: Fixed,
        fx: Option<Fixed>,
        level: usize,
        share: Option<i64>,
    ) -> Result<Priced, Reject> {
        let pos = self.accounts[&exec.account].position(exec.market);
        let toxicity_bps = if exec.liquidation { 0 } else { self.toxicity_add_on_bps(exec.account) };
        self.price_with(exec, pos, mid, fx, level, share, toxicity_bps)
    }

    /// The price for a fill against position `pos` at index `mid`, converting to USD at `fx`, at stress
    /// level `level`, best-price share `share` and toxicity add-on `toxicity_bps`, as `price_at` gives it
    /// for the account's position.
    /// Error cases:
    /// - as `price_at`
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn price_with(
        &self,
        exec: &Exec,
        pos: Position,
        mid: Fixed,
        fx: Option<Fixed>,
        level: usize,
        share: Option<i64>,
        toxicity_bps: i64,
    ) -> Result<Priced, Reject> {
        let params = self.params.as_ref().expect("callers check params are set");
        let market = &self.markets[&exec.market];
        let currency = market.spec.quote_currency;
        let usd = |amount, rounding| currency.to_usd(amount, fx, rounding);

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

        let stress = &params.stress_levels[level];
        let bands = AccountBands::at(&market.spec.spread_tiers, share);
        // From `V52` the slice that closes the position pays the size spread too.
        let finer = self.protocol_version >= ProtocolVersion::V52;
        let (close_from, close_to) = if finer { closing(pos.qty, delta) } else { (Fixed::ZERO, Fixed::ZERO) };
        // The spread per unit as its parts, every charge rounded as `charge` says: the stress and
        // toxicity charge over the whole quantity, and the size cost per unit, charged on the slice
        // that grows the position and the slice that closes it. Widening each slice outwards can
        // only add to its cost, and dividing the size cost by qty (not by notional) keeps full
        // precision.
        let parts = |charge: Rounding| {
            let slice_cost = |from: Fixed, to: Fixed| {
                let (from, to) = (usd(from.mul(mid), Rounding::Down), usd(to.mul_rounded(mid, charge), Rounding::Up));
                bands.staircase(from, to, charge)
            };
            let size_cost_usd = (slice_cost(grow_from, grow_to) + slice_cost(close_from, close_to))
                .mul_bps_rounded(stress.size_spread_multiplier_bps, charge);
            let size_cost = currency.from_usd(size_cost_usd, fx, Rounding::Up);
            let base = mid.mul_bps_rounded(stress.min_spread_bps + toxicity_bps, charge);
            (base, size_cost.div_rounded(qty, charge))
        };
        // The truncated spread decides whether a spread reaches the price. From `V10` the spread rounds
        // up, and a sell's stops a unit short of the price, so rounding never makes it reach the price.
        // From `V40`, a reversal over its ceiling is classified first: its refusal fills a triggered
        // order's closing slice, which stays executable when the blended spread reaches the price.
        // From `V52` a pure close's spread is clamped to its ceiling before the check, so it prices.
        let closes = !pos.qty.is_zero() && (exec.side == Side::Buy) != pos.qty.is_positive();
        let truncated = {
            let (base, size) = parts(Rounding::TowardZero);
            base + size
        };
        let over_ceiling = self.protocol_version >= ProtocolVersion::V40
            && closes
            && increases
            && self
                .close_spread_ceiling(exec.market)
                .is_some_and(|ceiling| truncated.raw() * 10_000 > mid.raw() * i128::from(ceiling));
        let checked = if finer && closes && !increases {
            self.bounded_close_charge(exec.market, exec.side, pos.qty, increases, truncated, mid)?
        } else {
            truncated
        };
        if !over_ceiling && checked >= mid {
            return Err(Reject::InvalidOrder { reason: "spread at this size reaches the price".into() });
        }
        let charge = self.charge_rounding();
        let (base, size) = parts(charge);
        // From `V40`, a close's spread cannot exceed its market's ceiling, and a reversal over it
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

    /// Books a priced fill: the position, from `V55` its open time, the market aggregates, realised PnL
    /// in USD (rounded down) and the profit fee at the closed slice's hold against house cash, the PnL
    /// in the account's emission period result, a markout to score the flow later
    /// (`Engine::mark_fill`), and from `V59` its notional in the account's volume (`Engine::count_volume`).
    fn book(&mut self, exec: &Exec, priced: Priced) -> Fill {
        let (hold, fee_bps) = self.close_fee_terms(exec.account, exec.market);
        let (version, now, clamp_hold) = (self.protocol_version, self.hold_clock_time(), self.add_clamp_hold());
        let market = self.markets.get_mut(&exec.market).expect("priced against this market");
        let acct = self.accounts.get_mut(&exec.account).expect("priced for this account");

        let pos = acct.positions.entry(exec.market).or_default();
        let held = pos.qty;
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
        holding_fee::move_open_time(version, &mut acct.opened_at, exec.market, (held, position), (now, clamp_hold));
        let (hold_ms, profit_fee_bps) = holding_fee::closed_terms(held, priced.delta, hold, fee_bps);
        let fee = profit_fee(realised_pnl, fee_bps, version);
        acct.balance += realised_pnl - fee;
        self.house_cash -= realised_pnl - fee;

        self.record_position(exec.account, exec.market);
        self.record_period_result(exec.account, realised_pnl, Fixed::ZERO);
        let mark = FillMark { held, qty: priced.qty, index: priced.mid, price: priced.price, fx: priced.fx };
        let closed_matched = self.mark_fill(exec, mark);
        self.count_volume(exec, &mark, closed_matched);
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
            hold_ms,
            profit_fee_bps,
            position,
            liquidation: exec.liquidation,
        }
    }

    /// How a spread charge or a margin requirement rounds: up from `V10`, so a trader never pays or
    /// posts less than the exact amount, and toward zero before.
    pub(crate) fn charge_rounding(&self) -> Rounding {
        if self.protocol_version >= ProtocolVersion::V10 { Rounding::Up } else { Rounding::TowardZero }
    }
}

mod batch;
mod cancel;
mod quote;
pub use quote::{Quote, Quoter};
mod risk;
use risk::HouseTotals;
mod trigger;
mod validation;

#[cfg(test)]
mod tests;
