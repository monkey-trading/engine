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
    pub(crate) staked: BTreeSet<AccountId>,
    pub(crate) distributions: BTreeSet<AccountId>,
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
    pub(super) fn staking_reference_leaves(&self, refs: BTreeSet<String>) -> Vec<(B256, Option<B256>)> {
        if self.protocol_version < crate::protocol::ProtocolVersion::V27 {
            return Vec::new();
        }
        refs.into_iter()
            .map(|reference| {
                (
                    crate::merkle::leaf::staking_reference_key(&reference),
                    self.staking_refs.contains(&reference).then(|| crate::merkle::leaf::credited_ref_hash(&reference)),
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
            staked: self.staked.take_touched(),
            distributions: self.distributions.take_touched(),
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
            staked: self.staked.keys().copied().collect(),
            distributions: self.distributions.keys().copied().collect(),
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
