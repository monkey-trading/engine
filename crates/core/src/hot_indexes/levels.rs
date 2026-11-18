//! The level index, from `V79` (`levels`): each take profit and stop loss by its key, the index mid
//! from which its position's closing price may reach it, ordered per market and side, so a price visits
//! only the levels its index reaches. It is derived, rebuilt with the other indexes, and each key is
//! recomputed whenever an input to its closing price changes (`levels::index`): the inputs each
//! market's and each account's keys were computed at are kept to tell.

use std::collections::{BTreeMap, BTreeSet};

use crate::Fixed;
use crate::levels::LevelKey;
use crate::spread_tier::SpreadTier;
use crate::toxicity::ScoreTier;
use crate::types::{AccountId, MarketId, Millis};
use crate::volume_tier::VolumeTier;

#[derive(Debug, Default)]
pub(crate) struct LevelIndex {
    pub(crate) markets: BTreeMap<MarketId, MarketLevels>,
    pub(crate) accounts: BTreeMap<AccountId, AccountLevels>,
    /// When an account's toxicity add-on may next change by decay alone, by sequenced time.
    pub(crate) decay_due: BTreeSet<(Millis, AccountId)>,
    /// When an account's volume tier may next change by time alone, by the hold clock.
    pub(crate) volume_due: BTreeSet<(Millis, AccountId)>,
    /// The tier tables the accounts' inputs were read under.
    pub(crate) tables: Option<(Vec<ScoreTier>, Vec<VolumeTier>)>,
}

/// What a market's closing prices are computed from, apart from each account's own inputs and position.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MarketInputs {
    pub(crate) stress_level: usize,
    pub(crate) min_spread_bps: i64,
    pub(crate) size_multiplier_bps: i64,
    pub(crate) spread_tiers: Vec<SpreadTier>,
    pub(crate) ceiling: Option<i64>,
}

/// What an account's closing prices are computed from, apart from each market's inputs and position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AccountInputs {
    pub(crate) toxicity_bps: i64,
    pub(crate) share: Option<i64>,
}

#[derive(Debug, Default)]
pub(crate) struct MarketLevels {
    /// The inputs every key here was computed at; `None` while the market cannot price a close.
    pub(crate) inputs: Option<MarketInputs>,
    /// Each account with levels here, and the keys of its take profit and stop loss.
    pub(crate) keys: BTreeMap<AccountId, [Option<LevelKey>; 2]>,
    /// Levels a mid at or below their key reaches.
    below: BTreeSet<(Fixed, AccountId)>,
    /// Levels a mid at or above their key reaches.
    above: BTreeSet<(Fixed, AccountId)>,
}

#[derive(Debug)]
pub(crate) struct AccountLevels {
    /// The inputs every key of the account was computed at.
    pub(crate) inputs: AccountInputs,
    pub(crate) markets: BTreeSet<MarketId>,
    pub(crate) decay_at: Option<Millis>,
    pub(crate) volume_at: Option<Millis>,
}

impl MarketLevels {
    /// No levels, keyed at `inputs`.
    pub(crate) fn new(inputs: Option<MarketInputs>) -> Self {
        MarketLevels { inputs, ..MarketLevels::default() }
    }

    /// Indexes `account`'s keys in place of any it had here.
    pub(crate) fn set(&mut self, account: AccountId, keys: [Option<LevelKey>; 2]) {
        self.unindex(account);
        for key in keys.iter().flatten() {
            let side = if key.below { &mut self.below } else { &mut self.above };
            side.insert((key.mid, account));
        }
        self.keys.insert(account, keys);
    }

    /// Forgets `account`'s keys here.
    pub(crate) fn remove(&mut self, account: AccountId) {
        self.unindex(account);
        self.keys.remove(&account);
    }

    /// Takes `account`'s keys out of the ordered sides.
    fn unindex(&mut self, account: AccountId) {
        for key in self.keys.get(&account).into_iter().flatten().flatten() {
            let side = if key.below { &mut self.below } else { &mut self.above };
            side.remove(&(key.mid, account));
        }
    }

    /// The accounts with a level whose key `mid` reaches, in account order.
    pub(crate) fn reached(&self, mid: Fixed) -> Vec<AccountId> {
        let (lowest, highest) = (AccountId([0; 20]), AccountId([0xff; 20]));
        let mut accounts: Vec<AccountId> = self
            .below
            .range((mid, lowest)..)
            .chain(self.above.range(..=(mid, highest)))
            .map(|(_, account)| *account)
            .collect();
        accounts.sort_unstable();
        accounts.dedup();
        accounts
    }
}
