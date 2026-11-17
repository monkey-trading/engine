//! Firing levels: on each price in their market, and when an entry's fill attaches them.

use crate::engine::Engine;
use crate::event::Event;
use crate::execution::PriceGuard;
use crate::levels::{FillCause, LevelKind};
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId};

#[cfg(test)]
thread_local! {
    /// Test-only, per test thread: whether a price prices every level in its market, ignoring the
    /// level index, so a test can compare the index's fills with pricing them all.
    pub(super) static UNCACHED_LEVELS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Engine {
    /// From `V79`, fires each level in `market` the closing price of its position reaches at the live
    /// index, in account order. The market's stress level is first brought to its inputs, as before a
    /// fill, so each level fires on the price its fill pays; a fill that moves the stress level starts
    /// the round again from the first account, as every closing price has moved. Nothing fires while the
    /// market cannot trade.
    pub(crate) fn trigger_levels(&mut self, market: MarketId, events: &mut Vec<Event>) {
        if self.protocol_version < ProtocolVersion::V79 || self.trigger_mid(market).is_none() {
            return;
        }
        self.ensure_hot_indexes();
        if !self.holds_levels(market) {
            return;
        }
        let nav = self.house_nav();
        self.refresh_stress(market, nav, events);
        'round: loop {
            self.sync_levels();
            self.sync_market_levels(market);
            let Some(mid) = self.trigger_mid(market) else { return };
            let level = self.markets[&market].stress.level;
            for account in self.level_candidates(market, mid) {
                let Some(kind) = self.reached_level(account, market) else { continue };
                self.fire_level(account, market, kind, events);
                let nav = self.house_nav();
                self.refresh_stress(market, nav, events);
                if self.markets[&market].stress.level != level {
                    continue 'round;
                }
            }
            return;
        }
    }

    /// From `V79`, fires a level of `account`'s position in `market` its closing price reaches now, after
    /// an entry's fill attached them, at the stress level the market's inputs set now.
    pub(crate) fn check_attached_levels(&mut self, account: AccountId, market: MarketId, events: &mut Vec<Event>) {
        if self.trigger_mid(market).is_none() {
            return;
        }
        let nav = self.house_nav();
        self.refresh_stress(market, nav, events);
        if let Some(kind) = self.reached_level(account, market) {
            self.fire_level(account, market, kind, events);
        }
    }

    /// Closes `account`'s whole position in `market` on its `kind` level: a stop loss at the closing price
    /// that reached it, a take profit at its level or better. The fill removes both levels.
    fn fire_level(&mut self, account: AccountId, market: MarketId, kind: LevelKind, events: &mut Vec<Event>) {
        let pos = self.accounts[&account].positions[&market];
        let price = self.accounts[&account].levels[&market].get(kind).expect("a reached level is set");
        let order_id = self.alloc_order_id();
        let mut exec = Self::closing_exec(account, market, pos, order_id);
        if kind == LevelKind::TakeProfit {
            exec.guard = PriceGuard::Limit(price);
        }
        // The close prices as it reached the level: nothing has moved since, and a pure close within
        // its ceiling always prices; a refusal leaves the level for the next price.
        if let Ok(fill) = self.execute(exec, events) {
            events.push(Event::PositionLevelTriggered { account, market, level: kind, price, order_id });
            self.after_fill(fill, FillCause::Level(kind), events);
        }
    }

    /// Whether any account holds levels in `market`.
    fn holds_levels(&self, market: MarketId) -> bool {
        #[cfg(test)]
        if UNCACHED_LEVELS.get() {
            return !self.uncached_holders(market).is_empty();
        }
        let index = &self.hot_indexes.as_ref().expect("built above").levels;
        index.markets.get(&market).is_some_and(|levels| !levels.keys.is_empty())
    }

    /// Test-only: the accounts holding levels in `market`, in account order, read from every account
    /// rather than the index.
    #[cfg(test)]
    fn uncached_holders(&self, market: MarketId) -> Vec<AccountId> {
        let holds = |(_, acct): &(&AccountId, &crate::account::Account)| acct.levels.contains_key(&market);
        self.accounts.iter().filter(holds).map(|(account, _)| *account).collect()
    }

    /// The accounts whose level in `market` index mid `mid` may reach, in account order: every holder in
    /// a test's uncached run, which checks the index against pricing them all.
    fn level_candidates(&self, market: MarketId, mid: crate::Fixed) -> Vec<AccountId> {
        #[cfg(test)]
        if UNCACHED_LEVELS.get() {
            return self.uncached_holders(market);
        }
        let index = &self.hot_indexes.as_ref().expect("built above").levels;
        index.markets.get(&market).map(|levels| levels.reached(mid)).unwrap_or_default()
    }
}
