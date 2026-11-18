//! What changed since the last state root: the tracked-map keys and the untracked sets.

use std::collections::BTreeSet;

use alloy_primitives::B256;

use crate::types::{AccountId, MarketId, OrderId};

/// Changes since the last root that no tracked map records. Positions change only in `book`,
/// and resting orders only where `place_order` rests one and `remove_order` takes one out; each
/// records what it changed here.
#[derive(Debug, Default)]
pub(crate) struct Changed {
    pub(crate) markouts: BTreeSet<u64>,
    pub(crate) positions: BTreeSet<(AccountId, MarketId)>,
    pub(crate) orders: BTreeSet<(AccountId, OrderId)>,
}

/// What computing a state root hashed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RootWork {
    /// Leaves re-encoded: those changed since the previous root, and the remaining-state leaf.
    pub leaves: usize,
    /// Tree nodes rehashed above the leaves whose hash changed.
    pub nodes: usize,
}

/// The keys of entries changed since the last root.
#[derive(Debug, Default)]
pub(crate) struct Touched {
    pub(crate) eligibility: BTreeSet<AccountId>,
    pub(crate) holds: BTreeSet<String>,
    pub(crate) pending: BTreeSet<String>,
    pub(crate) accounts: BTreeSet<AccountId>,
    pub(crate) markets: BTreeSet<MarketId>,
    pub(crate) nonces: BTreeSet<AccountId>,
    pub(crate) refs: BTreeSet<String>,
    pub(crate) staking_refs: BTreeSet<String>,
    pub(crate) period_results: BTreeSet<AccountId>,
    pub(crate) entitlements: BTreeSet<AccountId>,
    pub(crate) emission_ledgers: BTreeSet<AccountId>,
    pub(crate) staked: BTreeSet<AccountId>,
    pub(crate) distributions: BTreeSet<AccountId>,
    pub(crate) session_keys: BTreeSet<AccountId>,
    pub(crate) toxicity_scores: BTreeSet<AccountId>,
    pub(crate) markout_spreads: BTreeSet<MarketId>,
    pub(crate) volumes: BTreeSet<AccountId>,
    pub(crate) deposits: BTreeSet<String>,
    pub(crate) deposit_transactions: BTreeSet<String>,
    pub(crate) legacy_deposits: BTreeSet<String>,
    pub(crate) withdrawal_queue: BTreeSet<u64>,
    pub(crate) withdrawal_approvals: BTreeSet<B256>,
    pub(crate) corporate_actions: BTreeSet<String>,
    pub(crate) corporate_refs: BTreeSet<String>,
    pub(crate) changed: Changed,
}

impl crate::engine::Engine {
    /// Commits only changed staking identities under the protocol that authenticates them.
    pub(super) fn staking_reference_leaves(&self, refs: &BTreeSet<String>) -> Vec<(B256, Option<B256>)> {
        if self.protocol_version < crate::protocol::ProtocolVersion::V27 {
            return Vec::new();
        }
        refs.iter()
            .map(|reference| {
                (
                    crate::merkle::leaf::staking_reference_key(reference),
                    self.staking_refs.contains(reference).then(|| crate::merkle::leaf::credited_ref_hash(reference)),
                )
            })
            .collect()
    }
}

impl crate::engine::Engine {
    /// The keys of everything changed since the last root, clearing the record.
    pub(super) fn take_touched(&mut self) -> Touched {
        Touched {
            accounts: self.accounts.take_touched(),
            markets: self.markets.take_touched(),
            nonces: self.request_nonces.take_touched(),
            refs: self.credited_refs.take_touched(),
            staking_refs: self.staking_refs.take_touched(),
            period_results: self.period_results.take_touched(),
            entitlements: self.entitlements.take_touched(),
            emission_ledgers: self.emission_ledgers.take_touched(),
            staked: self.staked.take_touched(),
            distributions: self.distributions.take_touched(),
            session_keys: self.session_keys.take_touched(),
            toxicity_scores: self.toxicity_scores.take_touched(),
            markout_spreads: self.markout_spreads.take_touched(),
            volumes: self.volumes.take_touched(),
            deposits: self.deposits.take_touched(),
            deposit_transactions: self.deposit_transactions.take_touched(),
            legacy_deposits: self.legacy_deposits.take_touched(),
            withdrawal_queue: self.withdrawal_queue.take_touched(),
            withdrawal_approvals: self.withdrawal_approvals.take_touched(),
            corporate_actions: self.corporate_actions.take_touched(),
            corporate_refs: self.corporate_refs.take_touched(),
            eligibility: self.eligibility.take_touched(),
            holds: self.held_deposits.take_touched(),
            pending: self.pending_deposits.take_touched(),
            changed: std::mem::take(&mut self.changed),
        }
    }

    /// How many changed keys the engine holds for the next root. Grows with every applied command
    /// until a root is computed or the record is discarded.
    #[must_use]
    pub fn changed_key_count(&self) -> usize {
        self.accounts.touched_len()
            + self.markets.touched_len()
            + self.request_nonces.touched_len()
            + self.credited_refs.touched_len()
            + self.staking_refs.touched_len()
            + self.period_results.touched_len()
            + self.entitlements.touched_len()
            + self.emission_ledgers.touched_len()
            + self.staked.touched_len()
            + self.distributions.touched_len()
            + self.session_keys.touched_len()
            + self.toxicity_scores.touched_len()
            + self.markout_spreads.touched_len()
            + self.volumes.touched_len()
            + self.deposits.touched_len()
            + self.deposit_transactions.touched_len()
            + self.legacy_deposits.touched_len()
            + self.withdrawal_queue.touched_len()
            + self.withdrawal_approvals.touched_len()
            + self.corporate_actions.touched_len()
            + self.corporate_refs.touched_len()
            + self.eligibility.touched_len()
            + self.held_deposits.touched_len()
            + self.pending_deposits.touched_len()
            + self.changed.markouts.len()
            + self.changed.positions.len()
            + self.changed.orders.len()
    }

    /// Drops the record of what changed since the last root, for an engine that never computes a
    /// root, such as a read-only copy: with no root to drain it, the record would grow with every
    /// applied command. The next root then hashes every leaf, as for an engine rebuilt from its encoding.
    pub fn discard_changed_keys(&mut self) {
        self.tree = crate::merkle::state::Tree::None;
        self.clear_touched();
    }

    /// The key of every entry, for a root with no tree to update.
    pub(super) fn every_key(&self) -> Touched {
        let accounts = || self.accounts.iter();
        Touched {
            accounts: self.accounts.keys().copied().collect(),
            markets: self.markets.keys().copied().collect(),
            nonces: self.request_nonces.keys().copied().collect(),
            refs: self.credited_refs.iter().cloned().collect(),
            staking_refs: self.staking_refs.iter().cloned().collect(),
            period_results: self.period_results.keys().copied().collect(),
            entitlements: self.entitlements.keys().copied().collect(),
            emission_ledgers: self.emission_ledgers.keys().copied().collect(),
            staked: self.staked.keys().copied().collect(),
            distributions: self.distributions.keys().copied().collect(),
            session_keys: self.session_keys.keys().copied().collect(),
            toxicity_scores: self.toxicity_scores.keys().copied().collect(),
            markout_spreads: self.markout_spreads.keys().copied().collect(),
            volumes: self.volumes.keys().copied().collect(),
            deposits: self.deposits.keys().cloned().collect(),
            deposit_transactions: self.deposit_transactions.keys().cloned().collect(),
            legacy_deposits: self.legacy_deposits.keys().cloned().collect(),
            withdrawal_queue: self.withdrawal_queue.keys().copied().collect(),
            withdrawal_approvals: self.withdrawal_approvals.keys().copied().collect(),
            corporate_actions: self.corporate_actions.keys().cloned().collect(),
            corporate_refs: self.corporate_refs.iter().cloned().collect(),
            eligibility: self.eligibility.keys().copied().collect(),
            holds: self.held_deposits.keys().cloned().collect(),
            pending: self.pending_deposits.keys().cloned().collect(),
            changed: Changed {
                markouts: (self.markout_base..).take(self.markouts.len()).collect(),
                positions: accounts()
                    .flat_map(|(id, acct)| acct.positions.keys().map(|market| (*id, *market)))
                    .collect(),
                orders: accounts().flat_map(|(id, acct)| acct.orders.keys().map(|order_id| (*id, *order_id))).collect(),
            },
        }
    }
}

impl Touched {
    /// Adds `other`'s keys to these.
    pub(crate) fn absorb(&mut self, other: Touched) {
        self.eligibility.extend(other.eligibility);
        self.holds.extend(other.holds);
        self.pending.extend(other.pending);
        self.accounts.extend(other.accounts);
        self.markets.extend(other.markets);
        self.nonces.extend(other.nonces);
        self.refs.extend(other.refs);
        self.staking_refs.extend(other.staking_refs);
        self.period_results.extend(other.period_results);
        self.entitlements.extend(other.entitlements);
        self.emission_ledgers.extend(other.emission_ledgers);
        self.staked.extend(other.staked);
        self.distributions.extend(other.distributions);
        self.session_keys.extend(other.session_keys);
        self.toxicity_scores.extend(other.toxicity_scores);
        self.markout_spreads.extend(other.markout_spreads);
        self.volumes.extend(other.volumes);
        self.deposits.extend(other.deposits);
        self.deposit_transactions.extend(other.deposit_transactions);
        self.legacy_deposits.extend(other.legacy_deposits);
        self.withdrawal_queue.extend(other.withdrawal_queue);
        self.withdrawal_approvals.extend(other.withdrawal_approvals);
        self.corporate_actions.extend(other.corporate_actions);
        self.corporate_refs.extend(other.corporate_refs);
        self.changed.markouts.extend(other.changed.markouts);
        self.changed.positions.extend(other.changed.positions);
        self.changed.orders.extend(other.changed.orders);
    }
}

impl crate::engine::Engine {
    /// Up to `budget` keys from the record of what changed since the last root, and fewer once it
    /// runs out, taken kind by kind in a fixed order, positions first; the markets, which every
    /// price changes, stay for the root.
    pub(super) fn take_touched_part(&mut self, budget: usize) -> Touched {
        let mut left = budget;
        let mut part = Touched::default();
        part.changed.positions = take_first(&mut self.changed.positions, &mut left);
        part.accounts = take_first(self.accounts.touched_mut(), &mut left);
        part.nonces = take_first(self.request_nonces.touched_mut(), &mut left);
        part.refs = take_first(self.credited_refs.touched_mut(), &mut left);
        part.staking_refs = take_first(self.staking_refs.touched_mut(), &mut left);
        part.period_results = take_first(self.period_results.touched_mut(), &mut left);
        part.entitlements = take_first(self.entitlements.touched_mut(), &mut left);
        part.emission_ledgers = take_first(self.emission_ledgers.touched_mut(), &mut left);
        part.staked = take_first(self.staked.touched_mut(), &mut left);
        part.distributions = take_first(self.distributions.touched_mut(), &mut left);
        part.session_keys = take_first(self.session_keys.touched_mut(), &mut left);
        part.toxicity_scores = take_first(self.toxicity_scores.touched_mut(), &mut left);
        part.markout_spreads = take_first(self.markout_spreads.touched_mut(), &mut left);
        part.volumes = take_first(self.volumes.touched_mut(), &mut left);
        part.deposits = take_first(self.deposits.touched_mut(), &mut left);
        part.deposit_transactions = take_first(self.deposit_transactions.touched_mut(), &mut left);
        part.legacy_deposits = take_first(self.legacy_deposits.touched_mut(), &mut left);
        part.withdrawal_queue = take_first(self.withdrawal_queue.touched_mut(), &mut left);
        part.withdrawal_approvals = take_first(self.withdrawal_approvals.touched_mut(), &mut left);
        part.corporate_actions = take_first(self.corporate_actions.touched_mut(), &mut left);
        part.corporate_refs = take_first(self.corporate_refs.touched_mut(), &mut left);
        part.eligibility = take_first(self.eligibility.touched_mut(), &mut left);
        part.holds = take_first(self.held_deposits.touched_mut(), &mut left);
        part.pending = take_first(self.pending_deposits.touched_mut(), &mut left);
        part.changed.markouts = take_first(&mut self.changed.markouts, &mut left);
        part.changed.orders = take_first(&mut self.changed.orders, &mut left);
        part
    }

    /// Puts `touched` back in the record of what changed since the last root, so the next root
    /// computes those leaves again.
    pub(crate) fn record_touched(&mut self, touched: Touched) {
        self.eligibility.touched_mut().extend(touched.eligibility);
        self.held_deposits.touched_mut().extend(touched.holds);
        self.pending_deposits.touched_mut().extend(touched.pending);
        self.accounts.touched_mut().extend(touched.accounts);
        self.markets.touched_mut().extend(touched.markets);
        self.request_nonces.touched_mut().extend(touched.nonces);
        self.credited_refs.touched_mut().extend(touched.refs);
        self.staking_refs.touched_mut().extend(touched.staking_refs);
        self.period_results.touched_mut().extend(touched.period_results);
        self.entitlements.touched_mut().extend(touched.entitlements);
        self.emission_ledgers.touched_mut().extend(touched.emission_ledgers);
        self.staked.touched_mut().extend(touched.staked);
        self.distributions.touched_mut().extend(touched.distributions);
        self.session_keys.touched_mut().extend(touched.session_keys);
        self.toxicity_scores.touched_mut().extend(touched.toxicity_scores);
        self.markout_spreads.touched_mut().extend(touched.markout_spreads);
        self.volumes.touched_mut().extend(touched.volumes);
        self.deposits.touched_mut().extend(touched.deposits);
        self.deposit_transactions.touched_mut().extend(touched.deposit_transactions);
        self.legacy_deposits.touched_mut().extend(touched.legacy_deposits);
        self.withdrawal_queue.touched_mut().extend(touched.withdrawal_queue);
        self.withdrawal_approvals.touched_mut().extend(touched.withdrawal_approvals);
        self.corporate_actions.touched_mut().extend(touched.corporate_actions);
        self.corporate_refs.touched_mut().extend(touched.corporate_refs);
        self.changed.markouts.extend(touched.changed.markouts);
        self.changed.positions.extend(touched.changed.positions);
        self.changed.orders.extend(touched.changed.orders);
    }
}

/// The first `left` keys of `set`, or all of it when it holds no more, counted off `left`.
fn take_first<T: Ord + Clone>(set: &mut BTreeSet<T>, left: &mut usize) -> BTreeSet<T> {
    if *left >= set.len() {
        *left -= set.len();
        return std::mem::take(set);
    }
    let Some(first_kept) = set.iter().nth(*left).cloned() else { unreachable!("`left` is below the set's length") };
    let kept = set.split_off(&first_kept);
    *left = 0;
    std::mem::replace(set, kept)
}
