//! Account close-out and negative balance protection.
//!
//! Margin is shared across an account's positions. When equity falls below the close-out level of
//! the margin requirement, the largest losing position in USD that can trade right now is closed,
//! and so on until the account is back above the level; positions in markets that cannot trade wait
//! until they price again. A loss beyond the account's balance is written off against the house once
//! the account is flat.
//!
//! A price update checks the accounts holding its market. From `V6`, financing and parameter or
//! listing changes check accounts too, only a market inside its trading session can trade, and a
//! market that can trade counts at the index a close-out would fill it at. From `V12`, so does any
//! change to a market's index without a price update, before the next command acts. From `V81`, while
//! the parameters name an offset grid, the requirement is the portfolio requirement and a close-out
//! closes the least that restores the level, keeping each group together (`portfolio::closeout`).

use std::collections::BTreeMap;

use crate::Fixed;
use crate::account::Position;
use crate::closeout_index::Valuation;
use crate::engine::Engine;
use crate::event::Event;
use crate::execution::{Exec, PriceGuard};
use crate::fixed::Rounding;
use crate::levels::FillCause;
use crate::portfolio::CloseoutContext;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId, Side};

impl Engine {
    /// Closes out every account holding a position in `market_id` that has fallen below the
    /// close-out level, in account order. The close-out index names them (`closeout_index`), so the
    /// holders at or above their level are not valued.
    pub(crate) fn check_closeouts(&mut self, market_id: MarketId, events: &mut Vec<Event>) {
        let accounts = self.closeout_accounts(Some(market_id));
        self.close_out_each(accounts, events);
    }

    /// Closes out every account holding any position that has fallen below the close-out level, in
    /// account order, as the close-out index names them.
    pub(crate) fn check_all_closeouts(&mut self, events: &mut Vec<Event>) {
        let accounts = self.closeout_accounts(None);
        self.close_out_each(accounts, events);
    }

    /// Checks executable positions after a custody reversal, then settles a flat shortfall.
    pub(crate) fn close_out_after_balance_change(&mut self, account: AccountId, events: &mut Vec<Event>) {
        if let Some(params) = &self.params {
            self.close_out(account, params.closeout_level_bps, events);
        } else {
            self.settle_bad_debt(account, events);
        }
    }

    /// Writes off a flat account's negative balance against the house, and from `V19` records the
    /// write-off in the account's emission period result, and from `V83` counts it towards the account's
    /// reversed deposits (`deposits::write_offs`).
    pub(crate) fn settle_bad_debt(&mut self, account: AccountId, events: &mut Vec<Event>) {
        let Some(acct) = self.accounts.get(&account) else { return };
        if acct.positions.is_empty() && acct.balance.is_negative() {
            let amount = -acct.balance;
            self.accounts.get_mut(&account).expect("found above").balance = Fixed::ZERO;
            self.house_cash -= amount;
            self.record_period_result(account, Fixed::ZERO, amount);
            self.count_write_off(account, amount);
            events.push(Event::BadDebt { account, amount });
        }
    }

    /// Closes out each of `accounts`, in turn.
    fn close_out_each(&mut self, accounts: Vec<AccountId>, events: &mut Vec<Event>) {
        let level_bps = self.params.as_ref().expect("callers check params are set").closeout_level_bps;
        for account in accounts {
            #[cfg(feature = "closeout-oracle")]
            let before = self.oracle_before_close_out(account);
            self.close_out(account, level_bps, events);
            #[cfg(feature = "closeout-oracle")]
            self.oracle_after_close_out(account, before);
        }
    }

    /// Closes the account's positions, largest loser first, while it is below `level_bps` of its
    /// margin requirement, then settles any bad debt; from `V81`, while a grid applies, step by step
    /// under portfolio margin (`Engine::close_out_portfolio`).
    fn close_out(&mut self, account: AccountId, level_bps: i64, events: &mut Vec<Event>) {
        if self.portfolio_rules().is_some() {
            return self.close_out_portfolio(account, CloseoutContext::Price { level_bps }, events);
        }
        let mut announced = false;
        while let Some((market, pos, equity, margin)) = self.next_closeout(account, level_bps) {
            if !announced {
                events.push(Event::CloseOut { account, equity, margin });
                announced = true;
            }
            let order_id = self.alloc_order_id();
            let exec = Exec {
                account,
                order_id,
                market,
                side: if pos.qty.is_positive() { Side::Sell } else { Side::Buy },
                qty: pos.qty.abs(),
                reduce_only: true,
                guard: PriceGuard::None,
                liquidation: true,
                close_checked: false,
            };
            match self.execute(exec, events) {
                Ok(fill) => self.after_fill(fill, FillCause::CloseOut, events),
                Err(_) => break,
            }
        }
        self.settle_bad_debt(account, events);
    }

    /// Whether a close-out of `account` at `level_bps` would close anything now, under the rules in
    /// force: `next_closeout`, or from `V81`, while a grid applies, `next_portfolio_step`.
    #[cfg_attr(not(feature = "closeout-oracle"), allow(dead_code))]
    pub(crate) fn closes_out(&self, account: AccountId, level_bps: i64) -> bool {
        match self.portfolio_rules() {
            Some(_) => self.next_portfolio_step(account, CloseoutContext::Price { level_bps }).is_some(),
            None => self.next_closeout(account, level_bps).is_some(),
        }
    }

    /// The position to close next, with the account's equity and margin, or `None` when the
    /// account is at or above the close-out level or nothing it holds can trade now. Positions
    /// count at their last marks; from `V6`, one in a market that can trade counts at the index it
    /// would fill at, so the account is judged and ranked at the prices it is closed at. The rule
    /// before `V81`, and from it while no grid applies.
    pub(crate) fn next_closeout(
        &self,
        account: AccountId,
        level_bps: i64,
    ) -> Option<(MarketId, Position, Fixed, Fixed)> {
        let acct = self.accounts.get(&account)?;
        let at_index = self.protocol_version >= ProtocolVersion::V6;
        // The index each held market could fill at now, for those that can trade.
        let tradeable = || -> BTreeMap<MarketId, (Fixed, Option<Fixed>)> {
            acct.positions.keys().filter_map(|id| self.tradeable_index(*id).map(|index| (*id, index))).collect()
        };
        let (indexes, (equity, margin)) = if at_index {
            let indexes = tradeable();
            // The close-out index values accounts by the same `Valuation`, so the two cannot drift.
            let valued = self.equity_and_margin_at(acct, |market_id, market, pos| {
                Valuation::at(market, indexes.get(&market_id).copied()).price_for(pos)
            });
            (Some(indexes), valued)
        } else {
            (None, self.equity_and_margin(acct, None))
        };
        if margin.is_zero() || equity >= margin.mul_bps_rounded(level_bps, self.charge_rounding()) {
            return None;
        }
        let indexes = indexes.unwrap_or_else(tradeable);
        acct.positions
            .iter()
            .filter_map(|(market_id, pos)| {
                let (index, live_fx) = *indexes.get(market_id)?;
                let market = &self.markets[market_id];
                let (price, fx) = if at_index { (index, live_fx) } else { (market.mark?, market.fx_mark) };
                let pnl =
                    market.spec.quote_currency.to_usd(pos.unrealised(price, self.protocol_version), fx, Rounding::Down);
                Some((pnl, *market_id, *pos))
            })
            .min_by_key(|(upnl, market_id, _)| (*upnl, *market_id))
            .map(|(_, market_id, pos)| (market_id, pos, equity, margin))
    }

    /// The index a fill in `market_id` would take now, with the FX index it would convert at, or
    /// `None` unless the market is open with a valid index and an FX price when it is quoted in
    /// another currency than USD. From `V6`, the market must also be inside its session now, as its
    /// open flag changes only when its session is refreshed, and a close-out can run before that.
    pub(crate) fn tradeable_index(&self, market_id: MarketId) -> Option<(Fixed, Option<Fixed>)> {
        let in_session =
            self.protocol_version < ProtocolVersion::V6 || self.markets[&market_id].spec.schedule.is_open(self.now);
        if !in_session {
            return None;
        }
        let mid = self.live_index(market_id)?.mid;
        Some((mid, self.live_fx(market_id).ok()?))
    }
}
