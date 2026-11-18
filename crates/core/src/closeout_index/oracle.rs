//! Test-only hooks on the close-out index, behind the `closeout-oracle` feature, which only this
//! crate's own tests enable: today's full check in place of the index, the index checked against a
//! full assessment at every refresh and check site, and counts of what it did.

use std::collections::{BTreeMap, BTreeSet};

use crate::Fixed;
use crate::account::{Account, Position};
use crate::closeout_index::valuation::Valuation;
use crate::closeout_index::{Noted, Standing};
use crate::engine::Engine;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId, Millis};

/// How an engine's close-out checks run under test.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloseoutOracle {
    /// Every check site acts on every account today's full check visits, without the index: the
    /// market's holders, every account with positions, or every holder of a closing market.
    pub full_check: bool,
    /// The index is checked against a full assessment of every account at the end of each refresh,
    /// each check site's accounts against the full check's, and each close-out against changing
    /// another account or a valuation.
    pub verify: bool,
    /// Positions each command may key in place of the index's own allowance, so a test reaches the
    /// queue carried from one command to the next.
    pub keying_allowance: Option<usize>,
}

/// What the index did, so a test cannot pass without reaching each part.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloseoutCoverage {
    pub builds: u64,
    pub price_crossings: u64,
    pub fx_crossings: u64,
    pub headroom_moves: u64,
    /// Keys moved to a new close-out level.
    pub relevels: u64,
    /// Keys moved to a market's new bands, weekend margin or currency.
    pub rebands: u64,
    pub keys: u64,
    /// Keyed holders a weekend gap's close-out did not value.
    pub weekend_skips: u64,
    /// Markets a weekend gap's close-out valued at marks other than the valuation their keys hold.
    pub weekend_revaluations: u64,
    /// Refreshes that left accounts queued for a later command.
    pub carried_over: u64,
    /// Accounts a weekend gap's close-out closed out, under `verify`.
    pub weekend_closeouts: u64,
    /// Calls a check site left out, under `verify`.
    pub skipped_calls: u64,
    /// Accounts a check site named below their level with nothing that can trade, under `verify`.
    pub below_untradeable: u64,
}

/// Each other account's balance and positions, and each listed market's valuation.
pub(crate) type Snapshot = (BTreeMap<AccountId, (Fixed, BTreeMap<MarketId, Position>)>, Vec<Valuation>);

impl CloseoutCoverage {
    /// Counts `noted`.
    pub(super) fn note(&mut self, noted: Noted) {
        let count = match noted {
            Noted::Build => &mut self.builds,
            Noted::PriceCrossing => &mut self.price_crossings,
            Noted::FxCrossing => &mut self.fx_crossings,
            Noted::HeadroomMove => &mut self.headroom_moves,
            Noted::Relevel => &mut self.relevels,
            Noted::Reband => &mut self.rebands,
            Noted::Keyed => &mut self.keys,
            Noted::WeekendSkip => &mut self.weekend_skips,
            Noted::WeekendRevaluation => &mut self.weekend_revaluations,
            Noted::CarriedOver => &mut self.carried_over,
        };
        *count += 1;
    }
}

impl Engine {
    /// Runs this engine's close-out checks as `oracle` says, from now on.
    pub fn set_closeout_oracle(&mut self, oracle: CloseoutOracle) {
        self.closeout_index.oracle = oracle;
    }

    /// What the close-out index has done so far.
    pub fn closeout_coverage(&self) -> CloseoutCoverage {
        self.closeout_index.coverage
    }

    /// The accounts today's full check visits at a check site: `market`'s holders, or every account
    /// with positions.
    pub(crate) fn full_check_accounts(&mut self, market: Option<MarketId>) -> Vec<AccountId> {
        match market {
            Some(id) => {
                self.ensure_hot_indexes();
                let holders = &self.hot_indexes.as_ref().expect("built above").holders;
                holders.get(&id).into_iter().flatten().copied().collect()
            }
            None => self.accounts.iter().filter(|(_, acct)| !acct.positions.is_empty()).map(|(id, _)| *id).collect(),
        }
    }

    /// Under `verify`, checks a check site's accounts, `below`, are among today's and leave out only
    /// accounts `next_closeout` finds nothing to close on.
    pub(crate) fn oracle_check_site(&mut self, market: Option<MarketId>, below: &[AccountId]) {
        if !self.closeout_index.oracle.verify {
            return;
        }
        let full = self.full_check_accounts(market);
        let level_bps = self.params.as_ref().expect("a check site has parameters").closeout_level_bps;
        let named: BTreeSet<AccountId> = below.iter().copied().collect();
        assert!(named.iter().all(|account| full.contains(account)), "the index named a non-holder: {below:?}");
        for account in full.iter().filter(|account| !named.contains(account)) {
            assert!(
                self.next_closeout(*account, level_bps).is_none(),
                "the index skipped {account:?}, which the full check closes out"
            );
        }
        let untradeable = below.iter().filter(|account| self.next_closeout(**account, level_bps).is_none()).count();
        let coverage = &mut self.closeout_index.coverage;
        coverage.skipped_calls += (full.len() - named.len()) as u64;
        coverage.below_untradeable += untradeable as u64;
    }

    /// Under `verify`, checks a weekend gap's close-out over `closing` at `at` acts on `candidates`
    /// among today's accounts and leaves out only accounts it would find meeting their requirement.
    pub(crate) fn oracle_weekend_site(&mut self, closing: &BTreeSet<MarketId>, at: Millis, candidates: &[AccountId]) {
        if !self.closeout_index.oracle.verify {
            return;
        }
        let full = self.weekend_holders(closing);
        assert!(
            candidates.iter().all(|account| full.contains(account)),
            "a weekend candidate holds none of {closing:?}"
        );
        for account in full.iter().filter(|account| !candidates.contains(account)) {
            assert!(
                self.next_weekend_closeout(*account, closing, at).is_none(),
                "the index skipped {account:?}, which the weekend close-out closes out"
            );
        }
        let closed = candidates.iter().filter(|account| self.next_weekend_closeout(**account, closing, at).is_some());
        let closed = closed.count() as u64;
        let coverage = &mut self.closeout_index.coverage;
        coverage.skipped_calls += (full.len() - candidates.len()) as u64;
        coverage.weekend_closeouts += closed;
    }

    /// Under `verify`, what a close-out of `account` must leave as it is.
    pub(crate) fn oracle_before_close_out(&self, account: AccountId) -> Option<Snapshot> {
        self.closeout_index.oracle.verify.then(|| self.snapshot(account))
    }

    /// Under `verify`, checks the close-out of `account` changed no other account and no valuation.
    pub(crate) fn oracle_after_close_out(&self, account: AccountId, before: Option<Snapshot>) {
        if let Some(before) = before {
            assert!(before == self.snapshot(account), "closing out {account:?} changed another account's inputs");
        }
    }

    /// Under `verify`, checks the index after a refresh against a full assessment of every account.
    pub(crate) fn oracle_refreshed(&self) {
        if !self.closeout_index.oracle.verify {
            return;
        }
        let index = &self.closeout_index;
        let level_bps = index.level_bps;
        assert_eq!(index.version, Some(self.protocol_version), "the index holds under the version in force");
        let held: BTreeSet<AccountId> =
            self.accounts.iter().filter(|(_, acct)| !acct.positions.is_empty()).map(|(id, _)| *id).collect();
        assert_eq!(
            index.accounts.keys().copied().collect::<BTreeSet<_>>(),
            held,
            "the index holds every account with positions"
        );
        for (account, standing) in &index.accounts {
            let acct = &self.accounts[account];
            let markets: Vec<MarketId> = acct.positions.keys().copied().collect();
            match standing {
                Standing::Keyed(key) => {
                    assert!(key.holds(acct) && key.balance == acct.balance, "{account:?}'s key is stale");
                    for keyed in &key.positions {
                        let valuation = self.closeout_valuation(keyed.market);
                        assert!(
                            keyed.contains(valuation),
                            "{account:?}'s range in {} leaves {valuation:?}",
                            keyed.market
                        );
                        let keys = &index.markets[&keyed.market];
                        assert_eq!(keys.seen, Some(valuation), "market {} records its valuation", keyed.market);
                        let weak = key.weekend_weak() && keys.rules.as_ref().is_some_and(|r| r.weekend_margin_bps > 0);
                        assert_eq!(keys.weekend_weak.contains(account), weak, "{account:?}'s weekend standing");
                    }
                    assert_eq!(
                        self.rebound(key, level_bps),
                        Some(key.headroom),
                        "{account:?}'s headroom is not its ranges'"
                    );
                    assert!(!self.full_below(acct, level_bps), "{account:?} is keyed below its level");
                }
                Standing::Unkeyed { markets: entered, below } => {
                    assert_eq!(*entered, markets, "{account:?} is entered under the markets it holds");
                    assert_eq!(*below, self.full_below(acct, level_bps), "{account:?}'s flag is not the full check's");
                    assert_eq!(index.below.contains(account), *below);
                    for id in entered {
                        assert!(index.markets[id].unkeyed.contains(account));
                        assert_eq!(index.markets[id].below.contains(account), *below);
                    }
                }
            }
        }
        let queued: BTreeSet<AccountId> = index.queue.iter().copied().collect();
        assert!(queued.len() == index.queue.len() && queued == index.queued, "the queue holds each account once");
        for (id, keys) in &index.markets {
            let keyed = |account: &AccountId| match index.accounts.get(account) {
                Some(Standing::Keyed(key)) => key.positions.iter().any(|pos| pos.market == *id),
                _ => false,
            };
            assert!(keys.keyed().all(|account| keyed(&account)), "market {id} holds a stale key");
            assert!(
                keys.unkeyed
                    .iter()
                    .all(|account| matches!(index.accounts.get(account), Some(Standing::Unkeyed { .. })))
            );
            assert!(keys.below.is_subset(&keys.unkeyed));
        }
    }

    /// Whether `acct` is below `level_bps` of its requirement as `next_closeout` judges it, from the
    /// engine's own valuation.
    fn full_below(&self, acct: &Account, level_bps: i64) -> bool {
        let (equity, margin) = if self.protocol_version >= ProtocolVersion::V6 {
            self.equity_and_margin_at(acct, |id, market, pos| {
                self.tradeable_index(id).unwrap_or((market.mark.unwrap_or(pos.entry), market.fx_mark))
            })
        } else {
            self.equity_and_margin(acct, None)
        };
        !margin.is_zero() && equity < margin.mul_bps_rounded(level_bps, self.charge_rounding())
    }

    /// Every account's balance and positions but `account`'s, and every listed market's valuation.
    fn snapshot(&self, account: AccountId) -> Snapshot {
        let accounts = self
            .accounts
            .iter()
            .filter(|(id, _)| **id != account)
            .map(|(id, acct)| (*id, (acct.balance, acct.positions.clone())))
            .collect();
        (accounts, self.markets.keys().map(|id| self.closeout_valuation(*id)).collect())
    }
}
