//! Setting levels: the signed request, an order's levels, their checks, and what a fill does to them.

use crate::Fixed;
use crate::access::AccessEvidence;
use crate::account::Position;
use crate::auth::RequestScope;
use crate::command::{OrderKind, OrderRequest, PositionLevelsRequest};
use crate::eip712::position_levels_hash;
use crate::engine::Engine;
use crate::event::{Event, Reject};
use crate::execution::Priced;
use crate::fixed::Rounding;
use crate::levels::{FillCause, LevelKind, LevelsRemoved, PositionLevels};
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId, Side};

impl OrderRequest {
    /// The levels the order names, if any.
    pub fn levels(&self) -> Option<PositionLevels> {
        PositionLevels::named(self.take_profit, self.stop_loss)
    }
}

impl Engine {
    /// Authorises and screens the request, then sets the levels of the account's position in its market
    /// to exactly those it names, emitting `PositionLevelsSet`, or the rejection.
    /// Error cases:
    /// - before `V79`
    /// - the signature, nonce, or request expiry fails (see `Engine::authorise`), or access screening
    ///   refuses the account
    /// - an unknown market, a level at or below zero or above `Fixed::MAX_INPUT`, or no position
    /// - a new or moved level `Engine::check_level_distances` refuses against the position's closing
    ///   price (`Engine::levels_reference`)
    /// - a first level on the position with no slot free (`Engine::check_order_slot`)
    pub(crate) fn set_position_levels(
        &mut self,
        req: &PositionLevelsRequest,
        evidence: &AccessEvidence,
        events: &mut Vec<Event>,
    ) {
        let account = req.account;
        let reject = |reason, events: &mut Vec<Event>| {
            events.push(Event::OrderRejected { account, client_id: 0, reason });
        };
        if self.protocol_version < ProtocolVersion::V79 {
            return reject(Reject::LevelsUnsupported, events);
        }
        let authorised = self.authorise(account, req.auth.as_ref(), RequestScope::Trade, |domain, auth| {
            position_levels_hash(domain, req, auth.nonce, auth.expires_at)
        });
        let request_hash = self
            .access_domain()
            .zip(req.auth.as_ref())
            .map(|(domain, auth)| position_levels_hash(&domain, req, auth.nonce, auth.expires_at))
            .unwrap_or_default();
        let levels = PositionLevels { take_profit: req.take_profit, stop_loss: req.stop_loss };
        if let Err(reason) = authorised.and_then(|()| {
            self.check_access(Some(account), None, request_hash, Some(evidence), false, true)
                .map_err(|reason| Reject::Access { reason })
        }) {
            return reject(reason, events);
        }
        // A level is measured from the closing price a fill would pay now, so at the stress level the
        // market's inputs set now, as before a fill.
        if self.markets.contains_key(&req.market) && self.trigger_mid(req.market).is_some() {
            let nav = self.house_nav();
            self.refresh_stress(req.market, nav, events);
        }
        if let Err(reason) = self.check_level_request(account, req.market, levels) {
            return reject(reason, events);
        }
        self.write_levels(account, req.market, levels, events);
    }

    /// Checks a request may set `levels` on `account`'s position in `market`.
    /// Error cases:
    /// - as `Engine::set_position_levels` after screening
    fn check_level_request(&self, account: AccountId, market: MarketId, levels: PositionLevels) -> Result<(), Reject> {
        if !self.markets.contains_key(&market) {
            return Err(Reject::UnknownMarket);
        }
        check_level_inputs(levels)?;
        let Some(pos) = self.accounts.get(&account).and_then(|acct| acct.positions.get(&market)).copied() else {
            return Err(Reject::InvalidOrder { reason: "no position to set levels on".into() });
        };
        let held = self.accounts[&account].levels.get(&market).copied().unwrap_or_default();
        let moved = levels.moved_from(held);
        if !moved.is_empty() {
            let reference = self
                .levels_reference(account, market)
                .ok_or(Reject::InvalidOrder { reason: "no price to measure levels from".into() })?;
            self.check_level_distances(market, moved, pos.qty.is_positive(), reference)?;
        }
        if held.is_empty() && !levels.is_empty() {
            self.check_order_slot(account)?;
        }
        Ok(())
    }

    /// Checks the levels `req` names, at placement: from `V79`, named on an order that may open a position,
    /// each in range, and on a resting order each beyond the order's own price by its market's minimum
    /// distance on its own side. A market order's are checked against its fill (`check_order_levels`).
    /// Error cases:
    /// - levels before `V79`, or on a reduce-only order
    /// - a level at or below zero or above `Fixed::MAX_INPUT`
    /// - a resting order's level `Engine::check_level_distances` refuses against the order's price
    pub(crate) fn check_named_levels(&self, req: &OrderRequest) -> Result<(), Reject> {
        let Some(levels) = req.levels() else { return Ok(()) };
        if self.protocol_version < ProtocolVersion::V79 {
            return Err(Reject::LevelsUnsupported);
        }
        if req.reduce_only {
            return Err(Reject::InvalidOrder { reason: "a reduce-only order names no levels".into() });
        }
        check_level_inputs(levels)?;
        match req.kind {
            OrderKind::Market { .. } => Ok(()),
            OrderKind::Limit { price } | OrderKind::Stop { price } => {
                self.check_level_distances(req.market, levels, req.side == Side::Buy, price)
            }
        }
    }

    /// Checks the levels a market order names against the position its priced fill leaves on the order's
    /// side, before anything is booked: each new or moved level beyond that position's closing price at
    /// the fill's index by the market's minimum distance (an add keeps the position's levels, so one it
    /// names unchanged is not measured again), and a slot free for a position holding none. With no
    /// position left on the order's side there is nothing to check: the levels are dropped after the fill.
    /// Error cases:
    /// - a level `Engine::check_level_distances` refuses, or no slot free (`Engine::check_order_slot`)
    pub(crate) fn check_order_levels(
        &self,
        req: &OrderRequest,
        priced: &Priced,
        share: Option<i64>,
    ) -> Result<(), Reject> {
        let Some(levels) = req.levels() else { return Ok(()) };
        let held = self.accounts[&req.account].position(req.market);
        let qty = held.qty + priced.delta;
        if qty.is_zero() || qty.signum() != req.side.sign() {
            return Ok(());
        }
        // An add keeps the position's levels; a new position or a reversal takes only the order's.
        let added = !held.qty.is_zero() && held.qty.signum() == qty.signum();
        let kept = self.accounts[&req.account].levels.get(&req.market).copied().filter(|_| added);
        let moved = levels.moved_from(kept.unwrap_or_default());
        if !moved.is_empty() {
            let left = Position { qty, entry: priced.price };
            let level = self.markets[&req.market].stress.level;
            let toxicity = self.toxicity_add_on_bps(req.account);
            let reference =
                self.closing_price_at(req.account, req.market, left, priced.mid, priced.fx, level, share, toxicity)?;
            self.check_level_distances(req.market, moved, qty.is_positive(), reference)?;
        }
        if !self.accounts[&req.account].levels.contains_key(&req.market) {
            self.check_order_slot(req.account)?;
        }
        Ok(())
    }

    /// Checks each level in `levels` lies at least `market`'s minimum distance beyond `reference` on its
    /// own side, for a long position when `long`: a long's take profit above it and stop loss below it,
    /// and a short's the other way.
    /// Error cases:
    /// - a level short of that distance, or on the other side, as `Reject::LevelTooClose`
    /// - a market with no minimum distance
    pub(crate) fn check_level_distances(
        &self,
        market: MarketId,
        levels: PositionLevels,
        long: bool,
        reference: Fixed,
    ) -> Result<(), Reject> {
        // Every market listed under `V79` names one, and its activation gives the others their ceiling;
        // a log with no ceiling registry may still hold one with none, which takes no levels.
        let Some(min_distance_bps) = self.markets[&market].spec.min_level_distance_bps else {
            return Err(Reject::InvalidOrder { reason: "the market names no minimum level distance".into() });
        };
        let distance = reference.mul_bps_rounded(min_distance_bps, Rounding::Up);
        let (above, below) = (reference + distance, reference - distance);
        let far = |kind: LevelKind, level: Fixed| match (kind, long) {
            (LevelKind::TakeProfit, true) | (LevelKind::StopLoss, false) => level >= above,
            (LevelKind::StopLoss, true) | (LevelKind::TakeProfit, false) => level <= below,
        };
        for kind in [LevelKind::TakeProfit, LevelKind::StopLoss] {
            if let Some(level) = levels.get(kind).filter(|level| !far(kind, *level)) {
                return Err(Reject::LevelTooClose { level, min_distance_bps, reference });
            }
        }
        Ok(())
    }

    /// Checks `account` has a resting-order slot free: from `V14` below the parameters' cap, from `V66`
    /// its volume tier's limit where the table names one, counting from `V79` each market holding levels.
    /// Error cases:
    /// - no slot free, as `Reject::TierOrderLimit` or `Reject::TooManyOrders`
    pub(crate) fn check_order_slot(&self, account: AccountId) -> Result<(), Reject> {
        let max_resting_orders = self.params.as_ref().expect("callers check params are set").max_resting_orders;
        let acct = &self.accounts[&account];
        let levels = if self.protocol_version >= ProtocolVersion::V79 { acct.levels.len() } else { 0 };
        let used = acct.orders.len() + levels;
        match self.order_limit(account) {
            Some((tier, max)) if used >= max => Err(Reject::TierOrderLimit { tier, max }),
            Some(_) => Ok(()),
            None if used >= max_resting_orders => Err(Reject::TooManyOrders { max: max_resting_orders }),
            None => Ok(()),
        }
    }

    /// Gives the position a fill on `side` left in `market` the levels `named` an order carried, each in
    /// place of its own and keeping one the order does not name, or drops them, with an event either way,
    /// when the fill left no position on that side. A reversal's fill has already removed the levels of
    /// the position it turned.
    pub(crate) fn attach_levels(
        &mut self,
        account: AccountId,
        market: MarketId,
        side: Side,
        named: Option<PositionLevels>,
        events: &mut Vec<Event>,
    ) {
        let Some(levels) = named else { return };
        let qty = self.accounts[&account].position(market).qty;
        if qty.is_zero() || qty.signum() != side.sign() {
            let PositionLevels { take_profit, stop_loss } = levels;
            let reason = LevelsRemoved::NotOpened;
            events.push(Event::PositionLevelsRemoved { account, market, take_profit, stop_loss, reason });
            return;
        }
        let held = self.accounts[&account].levels.get(&market).copied().unwrap_or_default();
        self.write_levels(account, market, levels.over(held), events);
    }

    /// Sets the levels of `account`'s position in `market` to `levels`, removing the entry when neither is
    /// set, re-indexes them and emits `PositionLevelsSet`.
    fn write_levels(&mut self, account: AccountId, market: MarketId, levels: PositionLevels, events: &mut Vec<Event>) {
        let acct = self.accounts.get_mut(&account).expect("a position's account exists");
        if levels.is_empty() {
            acct.levels.remove(&market);
        } else {
            acct.levels.insert(market, levels);
        }
        self.record_levels(account, market);
        let PositionLevels { take_profit, stop_loss } = levels;
        events.push(Event::PositionLevelsSet { account, market, take_profit, stop_loss });
    }

    /// After a fill took `account`'s position in `market` from `held` to `left`: removes its levels, with
    /// their event, when the fill closed it or turned it to the other side, and re-indexes those it keeps
    /// over the new size.
    pub(crate) fn settle_levels(
        &mut self,
        account: AccountId,
        market: MarketId,
        held: Fixed,
        left: Fixed,
        cause: FillCause,
        events: &mut Vec<Event>,
    ) {
        let reason = if left.is_zero() {
            cause.removal()
        } else if left.signum() != held.signum() {
            LevelsRemoved::Reversed
        } else {
            self.record_levels(account, market);
            return;
        };
        self.remove_levels(account, market, reason, events);
    }

    /// Removes the levels of `account`'s position in `market`, if it holds any, with their event.
    pub(crate) fn remove_levels(
        &mut self,
        account: AccountId,
        market: MarketId,
        reason: LevelsRemoved,
        events: &mut Vec<Event>,
    ) {
        let Some(levels) = self.accounts.get(&account).and_then(|acct| acct.levels.get(&market)).copied() else {
            return;
        };
        self.accounts.get_mut(&account).expect("found above").levels.remove(&market);
        self.record_levels(account, market);
        let PositionLevels { take_profit, stop_loss } = levels;
        events.push(Event::PositionLevelsRemoved { account, market, take_profit, stop_loss, reason });
    }
}

/// Checks each level is a valid input price.
/// Error cases:
/// - a level at or below zero or above `Fixed::MAX_INPUT`
fn check_level_inputs(levels: PositionLevels) -> Result<(), Reject> {
    let valid = |level: Option<Fixed>| level.is_none_or(Fixed::is_valid_input);
    if valid(levels.take_profit) && valid(levels.stop_loss) {
        return Ok(());
    }
    Err(Reject::InvalidOrder { reason: "level out of range".into() })
}
