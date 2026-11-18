//! Keeping the level index exact (`hot_indexes::levels`). A key depends on its position, its account's
//! toxicity add-on and best-price share, and its market's stress level and its spread there, spread
//! bands and close-spread ceiling; in a market converting at another market's index it is bounded
//! without the FX index (`levels::keys`), so an FX move changes no key. Each is recomputed when one of
//! those changes:
//! - a fill or a level change re-keys its position (`Engine::record_levels`);
//! - an account's inputs are read again after every fill and markout of it, and whenever the time its
//!   volume book or decaying score could next move them comes: the next lot to count, or each UTC
//!   midnight while the book holds any volume, on the hold clock, and the next decay step on sequenced
//!   time; a change re-keys its levels;
//! - a market's inputs are read again before a price visits its levels, and before any of its keys is
//!   computed; a change re-keys its levels;
//! - a change of the tier tables reads every account's inputs again.
//!
//! Every key of an account is computed at the inputs recorded for it, and every key in a market at the
//! inputs recorded for the market, so a recorded input that matches the live one says its keys do too.

use std::cell::Cell;

use crate::Fixed;
use crate::engine::Engine;
use crate::hot_indexes::{AccountInputs, AccountLevels, MarketInputs, MarketLevels};
use crate::levels::keys::Spreads;
use crate::levels::{LevelKey, LevelKind};
use crate::protocol::ProtocolVersion;
use crate::toxicity::{HORIZON_MS, MIN_FILLS, STEP_MS, step_of};
use crate::types::{AccountId, MarketId, Millis};
use crate::volume_tier::DAY_MS;

/// The lowest index mid, in raw units, at which a market converting at its own index has the USD
/// notional of a close at one of two values: above one unit, `ceil(ceil(q m) / m)` is `q` or `q + 1`.
const SELF_FX_FLOOR: i128 = Fixed::ONE.raw() + 1;

impl Engine {
    /// Records that the levels of `account`'s position in `market`, or that position, changed: for the
    /// state root, and from `V79` re-keys them.
    pub(crate) fn record_levels(&mut self, account: AccountId, market: MarketId) {
        self.changed.positions.insert((account, market));
        self.key_levels(account, market);
    }

    /// From `V79`, re-keys the levels of `account`'s position in `market`, or takes them out of the index
    /// once the account holds none there.
    fn key_levels(&mut self, account: AccountId, market: MarketId) {
        if self.protocol_version < ProtocolVersion::V79 || self.hot_indexes.is_none() {
            return;
        }
        self.sync_levels();
        self.sync_market_levels(market);
        let held = self.accounts.get(&account).is_some_and(|acct| acct.levels.contains_key(&market));
        let index = &mut self.hot_indexes.as_mut().expect("checked above").levels;
        if !held {
            if let Some(levels) = index.markets.get_mut(&market) {
                levels.remove(account);
            }
            let emptied = index.accounts.get_mut(&account).is_some_and(|entry| {
                entry.markets.remove(&market);
                entry.markets.is_empty()
            });
            if emptied {
                self.forget_level_account(account);
            }
            return;
        }
        if !index.accounts.contains_key(&account) {
            let inputs = self.account_level_inputs(account);
            let entry = AccountLevels { inputs, markets: Default::default(), decay_at: None, volume_at: None };
            self.hot_indexes.as_mut().expect("checked above").levels.accounts.insert(account, entry);
            self.schedule_level_checks(account);
        }
        let index = &mut self.hot_indexes.as_mut().expect("checked above").levels;
        index.accounts.get_mut(&account).expect("inserted above").markets.insert(market);
        self.rekey_level(account, market);
    }

    /// From `V79`, reads `account`'s inputs again after a fill or markout changed its volume book or
    /// score, re-keying its levels if they moved.
    pub(crate) fn refresh_level_inputs(&mut self, account: AccountId) {
        if self.protocol_version < ProtocolVersion::V79 || self.hot_indexes.is_none() {
            return;
        }
        self.sync_levels();
        self.refresh_level_account(account);
    }

    /// Reads every account's inputs again when the tier tables changed, and those of each account whose
    /// time to be read again has come.
    pub(crate) fn sync_levels(&mut self) {
        let Some(indexes) = self.hot_indexes.as_ref() else { return };
        let held = indexes.levels.tables.as_ref().map(|(score, volume)| (score.as_slice(), volume.as_slice()));
        let live = self.params.as_ref().map(|_| (self.score_tiers(), self.volume_tiers_in_force().unwrap_or_default()));
        if held != live {
            self.hot_indexes.as_mut().expect("checked above").levels.tables = self.level_tables();
            let accounts: Vec<AccountId> =
                self.hot_indexes.as_ref().expect("checked above").levels.accounts.keys().copied().collect();
            accounts.into_iter().for_each(|account| self.refresh_level_account(account));
        }
        let (now, hold_clock) = (self.now, self.hold_clock_time());
        loop {
            let index = &mut self.hot_indexes.as_mut().expect("checked above").levels;
            let due = index.decay_due.first().filter(|(at, _)| *at <= now).copied();
            let due = due.or_else(|| index.volume_due.first().filter(|(at, _)| *at <= hold_clock).copied());
            let Some((_, account)) = due else { return };
            self.refresh_level_account(account);
        }
    }

    /// Reads `market`'s inputs again, re-keying every level in it if they moved.
    pub(crate) fn sync_market_levels(&mut self, market: MarketId) {
        let Some(indexes) = self.hot_indexes.as_ref() else { return };
        let inputs = self.market_level_inputs(market);
        let Some(levels) = indexes.levels.markets.get(&market) else {
            let levels = MarketLevels::new(inputs);
            self.hot_indexes.as_mut().expect("checked above").levels.markets.insert(market, levels);
            return;
        };
        if levels.inputs == inputs {
            return;
        }
        let accounts: Vec<AccountId> = levels.keys.keys().copied().collect();
        self.hot_indexes
            .as_mut()
            .expect("checked above")
            .levels
            .markets
            .get_mut(&market)
            .expect("found above")
            .inputs = inputs;
        accounts.into_iter().for_each(|account| self.rekey_level(account, market));
    }

    /// Rebuilds the level index from state, as the other indexes are rebuilt.
    pub(crate) fn index_all_levels(&mut self) {
        if self.protocol_version < ProtocolVersion::V79 {
            return;
        }
        let tables = self.level_tables();
        self.hot_indexes.as_mut().expect("built before the levels").levels.tables = tables;
        let held: Vec<(AccountId, MarketId)> = self
            .accounts
            .iter()
            .flat_map(|(account, acct)| acct.levels.keys().map(|market| (*account, *market)))
            .collect();
        held.into_iter().for_each(|(account, market)| self.key_levels(account, market));
    }

    /// Reads `account`'s inputs again, re-keying its levels if they moved, and schedules its next read.
    fn refresh_level_account(&mut self, account: AccountId) {
        let inputs = self.account_level_inputs(account);
        let index = &mut self.hot_indexes.as_mut().expect("callers check the index").levels;
        let Some(entry) = index.accounts.get_mut(&account) else { return };
        let markets: Vec<MarketId> = entry.markets.iter().copied().collect();
        let moved = entry.inputs != inputs;
        entry.inputs = inputs;
        self.schedule_level_checks(account);
        if moved {
            for market in markets {
                self.sync_market_levels(market);
                self.rekey_level(account, market);
            }
        }
    }

    /// Computes and indexes the keys of `account`'s levels in `market` at the inputs in force.
    fn rekey_level(&mut self, account: AccountId, market: MarketId) {
        let keys = self.level_keys(account, market);
        let index = &mut self.hot_indexes.as_mut().expect("callers check the index").levels;
        index.markets.entry(market).or_default().set(account, keys);
    }

    /// Takes `account` out of the index once it holds no levels.
    fn forget_level_account(&mut self, account: AccountId) {
        let index = &mut self.hot_indexes.as_mut().expect("callers check the index").levels;
        if let Some(entry) = index.accounts.remove(&account) {
            entry.decay_at.map(|at| index.decay_due.remove(&(at, account)));
            entry.volume_at.map(|at| index.volume_due.remove(&(at, account)));
        }
    }

    /// Schedules the next times `account`'s inputs may move by time alone, in place of those it had.
    fn schedule_level_checks(&mut self, account: AccountId) {
        let (decay_at, volume_at) = self.next_level_checks(account);
        let index = &mut self.hot_indexes.as_mut().expect("callers check the index").levels;
        let entry = index.accounts.get_mut(&account).expect("scheduled for an indexed account");
        entry.decay_at.map(|at| index.decay_due.remove(&(at, account)));
        entry.volume_at.map(|at| index.volume_due.remove(&(at, account)));
        (entry.decay_at, entry.volume_at) = (decay_at, volume_at);
        decay_at.map(|at| index.decay_due.insert((at, account)));
        volume_at.map(|at| index.volume_due.insert((at, account)));
    }

    /// The next sequenced time `account`'s toxicity add-on may move by decay, while its score holds the
    /// fills a tier needs, and the next hold-clock time its volume tier may move: its next lot to count,
    /// or while its book holds any volume, the next UTC midnight. A lot that has counted stays in its lot
    /// until a fill settles the book, so a book holding only lots counts volume a midnight can drop.
    fn next_level_checks(&self, account: AccountId) -> (Option<Millis>, Option<Millis>) {
        let (now, hold_clock) = (self.now, self.hold_clock_time());
        let decay = self
            .toxicity_scores
            .get(&account)
            .filter(|score| score.decayed_to(now).fills >= Fixed::from_int(MIN_FILLS))
            .map(|_| (step_of(now) + 1) * STEP_MS);
        let volume = self.volumes.get(&account).and_then(|book| {
            let opening = book.opening.values().flatten().map(|lot| lot.matures_at);
            let closing = book.closing.iter().map(|lot| lot.closed_at + HORIZON_MS);
            let early = book.early.iter().map(|lot| lot.due);
            let holds = !book.days.is_empty() || !book.opening.is_empty() || !book.closing.is_empty();
            let midnight = (holds || !book.early.is_empty()).then(|| (hold_clock.div_euclid(DAY_MS) + 1) * DAY_MS);
            opening.chain(closing).chain(early).filter(|at| *at > hold_clock).chain(midnight).min()
        });
        (decay, volume)
    }

    /// The tier tables an account's inputs are read under.
    fn level_tables(&self) -> Option<(Vec<crate::toxicity::ScoreTier>, Vec<crate::volume_tier::VolumeTier>)> {
        self.params.as_ref()?;
        let volume = self.volume_tiers_in_force().map(<[_]>::to_vec).unwrap_or_default();
        Some((self.score_tiers().to_vec(), volume))
    }

    /// `account`'s inputs to its closing prices.
    fn account_level_inputs(&self, account: AccountId) -> AccountInputs {
        AccountInputs { toxicity_bps: self.toxicity_add_on_bps(account), share: self.best_price_share(account) }
    }

    /// `market`'s inputs to its closing prices, or `None` before parameters are set.
    fn market_level_inputs(&self, market: MarketId) -> Option<MarketInputs> {
        let params = self.params.as_ref()?;
        let state = &self.markets[&market];
        let stress = &params.stress_levels[state.stress.level];
        Some(MarketInputs {
            stress_level: state.stress.level,
            min_spread_bps: stress.min_spread_bps,
            size_multiplier_bps: stress.size_spread_multiplier_bps,
            spread_tiers: state.spec.spread_tiers.clone(),
            ceiling: self.close_spread_ceiling(market),
        })
    }

    /// The keys of `account`'s take profit and stop loss in `market` at the inputs in force: none without
    /// levels or a position, or before parameters are set.
    fn level_keys(&self, account: AccountId, market: MarketId) -> [Option<LevelKey>; 2] {
        let Some(acct) = self.accounts.get(&account) else { return [None, None] };
        let (Some(levels), Some(pos)) = (acct.levels.get(&market), acct.positions.get(&market)) else {
            return [None, None];
        };
        let index = &self.hot_indexes.as_ref().expect("callers check the index").levels;
        let Some(inputs) = index.markets.get(&market).and_then(|levels| levels.inputs.as_ref()) else {
            return [None, None];
        };
        let (pos, long) = (*pos, pos.qty.is_positive());
        let fx_market = self.markets[&market].spec.quote_currency.fx_market();
        let (self_fx, cross) = (fx_market == Some(market), fx_market.is_some_and(|fx| fx != market));
        let (share, toxicity) = (self.best_price_share(account), self.toxicity_add_on_bps(account));
        let failed = Cell::new(false);
        let spread = |m: i128| {
            let mid = Fixed::from_raw(m);
            let fx = self_fx.then_some(mid);
            match self.closing_price_at(account, market, pos, mid, fx, inputs.stress_level, share, toxicity) {
                Ok(price) => (price - mid).raw().abs(),
                Err(_) => {
                    failed.set(true);
                    m
                }
            }
        };
        // A self-converting market's USD notional is `q + 1` units off a mid where `q m` is whole, and
        // `q` on one; the spread at the next mid off one bounds it from above. A market converting at
        // another's index is bounded by its ceiling from above, whatever the FX index. From below, no fill
        // pays less than the market's least fill rate (`Engine::least_fill_rate`).
        let q = pos.qty.raw().abs();
        let whole = |m: i128| (q % Fixed::ONE.raw()) * (m % Fixed::ONE.raw()) % Fixed::ONE.raw() == 0;
        let all_whole = q % Fixed::ONE.raw() == 0;
        let (least, ceiling) =
            (i128::from(self.least_fill_rate(market)), inputs.ceiling.map_or(99_999, |bps| bps * 10));
        let up = |m: i128| match () {
            _ if cross => m * i128::from(ceiling) / 100_000,
            _ if self_fx && !all_whole && whole(m) => spread(m + 1),
            _ => spread(m),
        };
        let down = |m: i128| if self_fx || cross { m * least / 100_000 } else { spread(m) };
        let spreads = Spreads { up, down, ceiling, floor: if self_fx { SELF_FX_FLOOR } else { 1 } };
        let key = |kind: LevelKind| {
            let level = levels.get(kind)?;
            match inputs.ceiling {
                Some(_) => spreads.key(long, kind, level),
                // With no ceiling bounding a close's spread, a price visits the level wherever it is.
                None => Some(widest(long, kind)),
            }
        };
        let keys = [key(LevelKind::TakeProfit), key(LevelKind::StopLoss)];
        if failed.get() {
            return [LevelKind::TakeProfit, LevelKind::StopLoss]
                .map(|kind| levels.get(kind).map(|_| widest(long, kind)));
        }
        keys
    }
}

#[cfg(test)]
impl Engine {
    /// Test-only: checks every level is indexed, that an account whose inputs moved has a read of them
    /// due, and that every key whose market's and account's recorded inputs are the live ones, with no
    /// read due, is the key those inputs give.
    pub(crate) fn assert_level_keys_current(&self) {
        let index = &self.hot_indexes.as_ref().expect("the index is built").levels;
        let (now, hold_clock) = (self.now, self.hold_clock_time());
        let due = |account: AccountId| {
            index.decay_due.iter().any(|(at, due)| *due == account && *at <= now)
                || index.volume_due.iter().any(|(at, due)| *due == account && *at <= hold_clock)
        };
        for (account, acct) in self.accounts.iter() {
            for market in acct.levels.keys() {
                let indexed = index.markets.get(market).is_some_and(|levels| levels.keys.contains_key(account));
                assert!(indexed, "{account:?}'s levels in market {market} are not indexed");
            }
        }
        let tables =
            self.params.as_ref().map(|_| (self.score_tiers(), self.volume_tiers_in_force().unwrap_or_default()));
        let current_tables =
            index.tables.as_ref().map(|(score, volume)| (score.as_slice(), volume.as_slice())) == tables;
        for (account, entry) in &index.accounts {
            if entry.inputs != self.account_level_inputs(*account) {
                assert!(due(*account) || !current_tables, "{account:?}'s inputs moved with no read of them due");
            }
        }
        for (market, levels) in &index.markets {
            if levels.inputs != self.market_level_inputs(*market) || !current_tables {
                continue;
            }
            for (account, keys) in &levels.keys {
                if due(*account) || index.accounts[account].inputs != self.account_level_inputs(*account) {
                    continue;
                }
                assert_eq!(*keys, self.level_keys(*account, *market), "{account:?}'s keys in market {market}");
            }
        }
    }
}

/// The key every valid mid reaches, for a level whose closing price cannot be bounded.
fn widest(long: bool, kind: LevelKind) -> LevelKey {
    let below = long == (kind == LevelKind::StopLoss);
    LevelKey { mid: if below { Fixed::MAX_INPUT } else { Fixed::from_raw(1) }, below }
}
