//! Building the close-out index, and bringing it to the state now at each check site.
//!
//! After a refresh, every account with positions is in the index, keyed or unkeyed. A keyed account's
//! key holds its balance and positions, its headroom is at or above zero at the close-out level and
//! bands in force, and the valuation of every market it holds lies inside its ranges. An unkeyed
//! account's flag is the exact check's verdict at the valuations now. Each input of the check that can
//! move between refreshes is caught: an account's balance and positions by the accounts' change record,
//! the level by comparison, a market's bands and currency by comparison with its listing, and its
//! valuation by computing every one afresh.

use std::collections::BTreeSet;

use crate::closeout_index::markets::MarketRules;
use crate::closeout_index::valuation::{Assessment, Valuations};
use crate::closeout_index::{CloseoutIndex, Noted, Standing};
use crate::engine::Engine;
use crate::fixed::Rounding;
use crate::types::{AccountId, MarketId};

impl CloseoutIndex {
    /// Builds the index from state: every account with positions assessed at the valuations now and
    /// keyed where it can be, with no allowance.
    pub(super) fn build(&mut self, engine: &Engine, level_bps: i64) {
        *self = CloseoutIndex {
            version: Some(engine.protocol_version),
            level_bps,
            coverage: std::mem::take(&mut self.coverage),
            #[cfg(feature = "closeout-oracle")]
            oracle: self.oracle,
            ..CloseoutIndex::default()
        };
        self.coverage.note(Noted::Build);
        let valuations = valuations(engine, engine.markets.keys());
        for (account, acct) in engine.accounts.iter().filter(|(_, acct)| !acct.positions.is_empty()) {
            let held = acct.positions.keys().copied().collect();
            self.enter_unkeyed(engine, *account, held);
            match engine.assess(acct, &valuations, level_bps) {
                Assessment::Below => self.set_below(*account, true),
                Assessment::Above { keyable: false } => {}
                Assessment::Above { keyable: true } => {
                    if let Some(key) = engine.key_account(acct, &valuations, level_bps) {
                        self.forget(*account);
                        self.enter_keyed(*account, key);
                    }
                }
            }
        }
        for (id, keys) in &mut self.markets {
            keys.seen = valuations.get(id).copied();
        }
    }

    /// Brings the index to the state now: the close-out level, each market's listing, the accounts
    /// `changed` since the last refresh, and each market's valuation, in that order; then assesses the
    /// accounts those unkeyed or touched, and keys queued accounts within the command's allowance.
    pub(super) fn refresh(&mut self, engine: &Engine, level_bps: i64, changed: BTreeSet<AccountId>) {
        if self.allowance_seq != engine.last_seq {
            (self.allowance, self.allowance_seq) = (self.keying_allowance(), engine.last_seq);
        }
        let charge = engine.charge_rounding();
        let mut marked = BTreeSet::new();
        if level_bps != self.level_bps {
            self.relevel(engine, level_bps, charge, &mut marked);
        }
        self.relist(engine, charge, &mut marked);
        for account in changed {
            self.account_changed(engine, account, charge, &mut marked);
        }
        let valuations = valuations(engine, self.markets.keys());
        self.revalue(engine, &valuations, &mut marked);
        for account in marked {
            self.reassess(engine, account, &valuations);
        }
        self.key_queued(engine, &valuations);
    }

    /// Moves every key to `level_bps`, unkeying those left below zero, and marks every unkeyed account.
    fn relevel(&mut self, engine: &Engine, level_bps: i64, charge: Rounding, marked: &mut BTreeSet<AccountId>) {
        self.level_bps = level_bps;
        self.coverage.note(Noted::Relevel);
        let mut broken = Vec::new();
        for (account, standing) in &mut self.accounts {
            match standing {
                Standing::Keyed(key) => {
                    if key.settle(level_bps, charge).is_none() {
                        broken.push(*account);
                    }
                }
                Standing::Unkeyed { .. } => {
                    marked.insert(*account);
                }
            }
        }
        for account in broken {
            self.unkey(engine, account, marked);
        }
    }

    /// Takes up each indexed market's listing where its bands, currency or weekend margin moved: a
    /// new currency unkeys every keyed holder, new bands move each keyed holder's requirement and
    /// unkey those left below zero, and the market's unkeyed holders are marked.
    fn relist(&mut self, engine: &Engine, charge: Rounding, marked: &mut BTreeSet<AccountId>) {
        let moved: Vec<(MarketId, Option<MarketRules>)> = self
            .markets
            .iter()
            .filter_map(|(id, keys)| {
                let spec = engine.markets.get(id).map(|market| &market.spec);
                let same = match (&keys.rules, spec) {
                    (Some(rules), Some(spec)) => rules.matches(spec),
                    (None, None) => true,
                    _ => false,
                };
                (!same).then(|| (*id, spec.map(MarketRules::of)))
            })
            .collect();
        for (id, new) in moved {
            let keys = self.markets.get_mut(&id).expect("listed above");
            let old = std::mem::replace(&mut keys.rules, new.clone());
            let holders: Vec<AccountId> = keys.keyed().collect();
            marked.extend(keys.unkeyed.iter().copied());
            let bands = match (&old, &new) {
                (Some(old), Some(new)) if old.quote_currency == new.quote_currency => Some((old.clone(), new.clone())),
                _ => None,
            };
            self.coverage.note(Noted::Reband);
            let level_bps = self.level_bps;
            for account in holders {
                let rebanded = bands.as_ref().and_then(|(old, new)| {
                    let Some(Standing::Keyed(key)) = self.accounts.get_mut(&account) else { return None };
                    key.reband(id, (old, new), level_bps, charge)
                });
                match rebanded {
                    Some(()) => self.place_weak(account),
                    None => self.unkey(engine, account, marked),
                }
            }
        }
    }

    /// Takes up a change to `account`: one that left it flat leaves the index, one to its balance alone
    /// moves its key's headroom, and any other unkeys it.
    fn account_changed(
        &mut self,
        engine: &Engine,
        account: AccountId,
        charge: Rounding,
        marked: &mut BTreeSet<AccountId>,
    ) {
        let Some(acct) = engine.accounts.get(&account).filter(|acct| !acct.positions.is_empty()) else {
            self.forget(account);
            return;
        };
        if let Some(Standing::Keyed(key)) = self.accounts.get_mut(&account)
            && key.holds(acct)
        {
            if key.balance == acct.balance {
                return;
            }
            self.coverage.note(Noted::HeadroomMove);
            let weak = key.weekend_weak();
            if key.rebalance(acct.balance, self.level_bps, charge).is_some() {
                if key.weekend_weak() != weak {
                    self.place_weak(account);
                }
                return;
            }
        }
        self.unkey(engine, account, marked);
    }

    /// Takes up each market's valuation: where it moved, unkeys the keys it left and marks the market's
    /// unkeyed holders.
    fn revalue(&mut self, engine: &Engine, valuations: &Valuations, marked: &mut BTreeSet<AccountId>) {
        for (id, valuation) in valuations {
            let keys = self.markets.get_mut(id).expect("valued markets are indexed");
            if keys.seen == Some(*valuation) {
                continue;
            }
            let (by_price, by_fx) = keys.crossed_by(*valuation);
            keys.seen = Some(*valuation);
            if !by_price.is_empty() {
                self.coverage.note(Noted::PriceCrossing);
            }
            if !by_fx.is_empty() {
                self.coverage.note(Noted::FxCrossing);
            }
            let crossed: BTreeSet<AccountId> = by_price.into_iter().chain(by_fx).collect();
            for account in crossed {
                self.unkey(engine, account, marked);
            }
            marked.extend(self.markets[id].unkeyed.iter().copied());
        }
    }

    /// Assesses the unkeyed `account` at `valuations`, queueing it to be keyed when it can be.
    fn reassess(&mut self, engine: &Engine, account: AccountId, valuations: &Valuations) {
        let (Some(Standing::Unkeyed { .. }), Some(acct)) = (self.accounts.get(&account), engine.accounts.get(&account))
        else {
            return;
        };
        match engine.assess(acct, valuations, self.level_bps) {
            Assessment::Below => self.set_below(account, true),
            Assessment::Above { keyable } => {
                self.set_below(account, false);
                if keyable && self.queued.insert(account) {
                    self.queue.push_back(account);
                }
            }
        }
    }

    /// Keys queued accounts while the command's allowance lasts, each charged a unit a position it
    /// holds, or one when its entry is dropped: it left the index, or is no longer unkeyed at or above
    /// its level. A command keys no more positions than the allowance, but for an account holding
    /// more than the whole of it, which is keyed alone as the command's first.
    fn key_queued(&mut self, engine: &Engine, valuations: &Valuations) {
        while let Some(&account) = self.queue.front() {
            let eligible = matches!(self.accounts.get(&account), Some(Standing::Unkeyed { below: false, .. }));
            let acct = engine.accounts.get(&account).filter(|_| eligible);
            let cost = acct.map_or(1, |acct| acct.positions.len().max(1));
            if cost > self.allowance && self.allowance < self.keying_allowance() {
                self.coverage.note(Noted::CarriedOver);
                break;
            }
            self.queue.pop_front();
            self.queued.remove(&account);
            self.allowance = self.allowance.saturating_sub(cost);
            if let Some(key) = acct.and_then(|acct| engine.key_account(acct, valuations, self.level_bps)) {
                self.forget(account);
                self.enter_keyed(account, key);
            }
        }
    }
}

/// The valuation of each listed market among `markets`.
fn valuations<'a>(engine: &Engine, markets: impl Iterator<Item = &'a MarketId>) -> Valuations {
    markets.filter(|id| engine.markets.contains_key(id)).map(|id| (*id, engine.closeout_valuation(*id))).collect()
}
