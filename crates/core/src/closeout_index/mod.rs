//! The close-out index: which accounts a close-out check must call `close_out` on, found without
//! valuing every account it covers. Derived from state, never encoded, and built at recovery and at
//! an activation, so it moves no root.
//!
//! Every account with positions is keyed or unkeyed. A keyed account has a range per position
//! (`key`) inside which it provably stays at or above its close-out level, and its markets hold it by
//! the ends of those ranges (`markets`). An unkeyed one is below its level, too near it to key, not
//! keyed yet, or not valued without an overflow, and carries the exact check's verdict
//! (`valuation`). Each check site first refreshes the index: every input of the decision that moved
//! since the last refresh, an account's balance or positions, a market's valuation, its bands or
//! currency, the close-out level or the protocol version, unkeys exactly the keys it may break and
//! re-values the unkeyed accounts it touches. The check site then calls `close_out` only on the
//! accounts below their level, in account order. A call it skips would find the account at or above
//! its level and do nothing, and no close-out changes another account's decision, so the calls it
//! makes, their events and the state they leave are today's. Keys bound the per-position requirement,
//! the one every close-out reads.
//!
//! A weekend gap's close-out uses the same keys: a keyed account whose valuation at its marks lies
//! inside its ranges, and whose key shows it meeting the requirement with every weekend margin, is
//! skipped there too. Keying spends a per-command allowance of positions, so after a mass event the
//! accounts left unkeyed are re-valued exactly at each move until the queue drains.

mod key;
mod markets;
#[cfg(feature = "closeout-oracle")]
mod oracle;
mod refresh;
mod valuation;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::closeout_index::key::Key;
use crate::closeout_index::markets::MarketKeys;
use crate::engine::Engine;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId};

#[cfg(feature = "closeout-oracle")]
pub use oracle::{CloseoutCoverage, CloseoutOracle};

/// Positions a command may key, across every refresh it runs, so the keying a mass event leaves is
/// spread over the commands after it.
const KEYING_ALLOWANCE: usize = 2_000;

/// The close-out index, built at the first check site and rebuilt when the protocol version moves.
#[derive(Debug, Default)]
pub(crate) struct CloseoutIndex {
    /// The protocol version the keys hold under; `None` until built.
    version: Option<ProtocolVersion>,
    /// The close-out level every key's headroom is at.
    level_bps: i64,
    markets: BTreeMap<MarketId, MarketKeys>,
    accounts: BTreeMap<AccountId, Standing>,
    /// The unkeyed accounts that must be closed out.
    below: BTreeSet<AccountId>,
    /// Unkeyed accounts at or above their level, to be keyed, oldest first; `queued` holds each once.
    queue: VecDeque<AccountId>,
    queued: BTreeSet<AccountId>,
    /// Positions left to key in the command `allowance_seq` names.
    allowance: usize,
    allowance_seq: Option<u64>,
    coverage: Coverage,
    #[cfg(feature = "closeout-oracle")]
    oracle: CloseoutOracle,
}

/// Where an account with positions stands in the index.
#[derive(Debug)]
enum Standing {
    Keyed(Box<Key>),
    /// The markets it is entered under, and whether it must be closed out.
    Unkeyed {
        markets: Vec<MarketId>,
        below: bool,
    },
}

/// What the index did, which the oracle counts so a test cannot pass without reaching it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Noted {
    Build,
    PriceCrossing,
    FxCrossing,
    HeadroomMove,
    Relevel,
    Reband,
    Keyed,
    WeekendSkip,
}

/// The counts the oracle reads; nothing without it.
#[cfg(not(feature = "closeout-oracle"))]
#[derive(Debug, Default)]
struct Coverage;

#[cfg(feature = "closeout-oracle")]
type Coverage = CloseoutCoverage;

#[cfg(not(feature = "closeout-oracle"))]
impl Coverage {
    /// Counts nothing.
    fn note(&mut self, _: Noted) {}
}

impl CloseoutIndex {
    /// Forgets `account`, removing it from every set it is in; any queue entry is dropped when taken.
    fn forget(&mut self, account: AccountId) {
        match self.accounts.remove(&account) {
            Some(Standing::Keyed(key)) => {
                for pos in &key.positions {
                    self.markets.get_mut(&pos.market).expect("a keyed market is indexed").leave(account, pos);
                }
            }
            Some(Standing::Unkeyed { markets, .. }) => {
                for id in markets {
                    let keys = self.markets.get_mut(&id).expect("an entered market is indexed");
                    keys.unkeyed.remove(&account);
                    keys.below.remove(&account);
                }
                self.below.remove(&account);
            }
            None => {}
        }
    }

    /// Enters `account` unkeyed under `markets`, at or above its level until it is assessed.
    fn enter_unkeyed(&mut self, engine: &Engine, account: AccountId, markets: Vec<MarketId>) {
        for id in &markets {
            let spec = engine.markets.get(id).map(|market| &market.spec);
            self.markets.entry(*id).or_insert_with(|| MarketKeys::new(spec)).unkeyed.insert(account);
        }
        self.accounts.insert(account, Standing::Unkeyed { markets, below: false });
    }

    /// Enters `account` keyed by `key`, whose markets are all indexed.
    fn enter_keyed(&mut self, account: AccountId, key: Key) {
        for pos in &key.positions {
            self.markets.get_mut(&pos.market).expect("a valued market is indexed").enter(account, pos);
        }
        self.accounts.insert(account, Standing::Keyed(Box::new(key)));
        self.place_weak(account);
        self.coverage.note(Noted::Keyed);
    }

    /// Unkeys `account`, entering it under the markets it holds now, and marks it to be assessed.
    fn unkey(&mut self, engine: &Engine, account: AccountId, marked: &mut BTreeSet<AccountId>) {
        self.forget(account);
        let held = engine.accounts.get(&account).map(|acct| acct.positions.keys().copied().collect::<Vec<_>>());
        if let Some(markets) = held.filter(|markets| !markets.is_empty()) {
            self.enter_unkeyed(engine, account, markets);
            marked.insert(account);
        }
    }

    /// Records whether the unkeyed `account` must be closed out.
    fn set_below(&mut self, account: AccountId, below: bool) {
        let Some(Standing::Unkeyed { markets, below: flag }) = self.accounts.get_mut(&account) else { return };
        *flag = below;
        for id in markets.iter() {
            let keys = self.markets.get_mut(id).expect("an entered market is indexed");
            if below {
                keys.below.insert(account)
            } else {
                keys.below.remove(&account)
            };
        }
        if below {
            self.below.insert(account)
        } else {
            self.below.remove(&account)
        };
    }

    /// Enters the keyed `account` in the weekend sets of its markets with a weekend margin while its key
    /// does not show it meeting the requirement with them, and removes it otherwise.
    fn place_weak(&mut self, account: AccountId) {
        let Some(Standing::Keyed(key)) = self.accounts.get(&account) else { return };
        let weak = key.weekend_weak();
        for pos in &key.positions {
            let keys = self.markets.get_mut(&pos.market).expect("a keyed market is indexed");
            let weekend = keys.rules.as_ref().is_some_and(|rules| rules.weekend_margin_bps > 0);
            if weak && weekend {
                keys.weekend_weak.insert(account)
            } else {
                keys.weekend_weak.remove(&account)
            };
        }
    }

    /// The keyed holders of a market among `closing`, the accounts a weekend gap's close-out over
    /// `closing` may skip: each whose valuation at its marks lies inside its ranges and whose key
    /// shows it meeting the requirement with every weekend margin. The rest are returned, in account
    /// order: every unkeyed holder, and every keyed one the key does not clear.
    fn weekend_candidates(&mut self, engine: &Engine, closing: &BTreeSet<MarketId>) -> Vec<AccountId> {
        let mut candidates = BTreeSet::new();
        let mut keyed = 0;
        for keys in closing.iter().filter_map(|id| self.markets.get(id)) {
            candidates.extend(keys.unkeyed.iter().copied());
            keyed += keys.keyed_count();
            match keys.rules.as_ref().is_some_and(|rules| rules.weekend_margin_bps > 0) {
                true => candidates.extend(keys.weekend_weak.iter().copied()),
                false => candidates.extend(keys.keyed()),
            }
        }
        let holds_closing = |account: &AccountId| match self.accounts.get(account) {
            Some(Standing::Keyed(key)) => key.positions.iter().any(|pos| closing.contains(&pos.market)),
            _ => false,
        };
        for (id, keys) in self.markets.iter().filter(|(id, _)| engine.markets.contains_key(id)) {
            let at_marks = engine.weekend_valuation(*id);
            if keys.seen != Some(at_marks) {
                candidates.extend(keys.crossed(at_marks).into_iter().filter(holds_closing));
            }
        }
        if keyed > candidates.len() {
            self.coverage.note(Noted::WeekendSkip);
        }
        candidates.into_iter().collect()
    }
}

impl Engine {
    /// The accounts a close-out check of `market`'s holders, or of every account with positions, must
    /// call `close_out` on, in account order: each below its level at the valuations now, or not valued
    /// without an overflow. The call on any other would find it at or above its level.
    pub(crate) fn closeout_accounts(&mut self, market: Option<MarketId>) -> Vec<AccountId> {
        #[cfg(feature = "closeout-oracle")]
        if self.closeout_index.oracle.full_check {
            return self.full_check_accounts(market);
        }
        self.refresh_closeout_index();
        let index = &self.closeout_index;
        let below = match market {
            Some(id) => index.markets.get(&id).map_or_else(Vec::new, |keys| keys.below.iter().copied().collect()),
            None => index.below.iter().copied().collect(),
        };
        #[cfg(feature = "closeout-oracle")]
        self.oracle_check_site(market, &below);
        below
    }

    /// The accounts a weekend gap's close-out over `closing` must call `weekend_close_out` on, in
    /// account order: every holder of a market in `closing` the index cannot show meeting the
    /// requirement with its weekend margins at its marks. Before parameters, when nothing is held,
    /// every holder.
    pub(crate) fn weekend_closeout_accounts(&mut self, closing: &BTreeSet<MarketId>) -> Vec<AccountId> {
        #[cfg(feature = "closeout-oracle")]
        let full_check = self.closeout_index.oracle.full_check;
        #[cfg(not(feature = "closeout-oracle"))]
        let full_check = false;
        if self.params.is_none() || full_check {
            return self.weekend_holders(closing);
        }
        self.refresh_closeout_index();
        let mut index = std::mem::take(&mut self.closeout_index);
        let candidates = index.weekend_candidates(self, closing);
        self.closeout_index = index;
        candidates
    }

    /// Every account holding a market in `closing`, in account order.
    pub(crate) fn weekend_holders(&self, closing: &BTreeSet<MarketId>) -> Vec<AccountId> {
        let holders = &self.hot_indexes.as_ref().expect("apply builds derived indexes").holders;
        let accounts: BTreeSet<AccountId> =
            closing.iter().flat_map(|id| holders.get(id).into_iter().flatten().copied()).collect();
        accounts.into_iter().collect()
    }

    /// Builds the close-out index afresh, keying every account it can: at recovery, and at an
    /// activation, whose rules the keys may not hold under.
    pub(crate) fn rebuild_closeout_index(&mut self) {
        self.closeout_index.version = None;
        if self.params.is_some() {
            self.refresh_closeout_index();
        }
    }

    /// Brings the index to the state now (`CloseoutIndex::refresh`), building it under a protocol
    /// version it was not built under. Parameters are set.
    fn refresh_closeout_index(&mut self) {
        let level_bps = self.params.as_ref().expect("callers check params are set").closeout_level_bps;
        let changed = self.accounts.take_watched();
        let mut index = std::mem::take(&mut self.closeout_index);
        if index.version == Some(self.protocol_version) {
            index.refresh(self, level_bps, changed);
        } else {
            index.build(self, level_bps);
        }
        self.closeout_index = index;
        #[cfg(feature = "closeout-oracle")]
        self.oracle_refreshed();
    }
}

#[cfg(test)]
mod tests;
