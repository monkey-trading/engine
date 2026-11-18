//! What a leaf carries beside its hash for the proofs that read it, the leaf writes a root is made
//! of, and the index of the exit's leaves by the account they name.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::B256;

use crate::Fixed;
use crate::account::Position;
use crate::emissions::TokenUnits;
use crate::levels::PositionLevels;
use crate::merkle::credits::unfinalised_credit_key;
use crate::merkle::{
    AccountLeaf, ExitTerms, MarketLeaf, PendingReceipt, held_deposit_key, pending_deposit_key, withdrawal_queue_key,
};
use crate::staking::UsdcUnits;
use crate::types::{AccountId, MarketId};
use crate::withdrawal::{QueuedWithdrawal, WithdrawalApproval};

/// A leaf's value, for the kinds proofs read; every other leaf carries its hash alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeafValue {
    /// An account, with the markets it holds positions in, its position leaves, and from `V79` those
    /// whose positions carry levels, its levels leaves, each in increasing order.
    Account {
        leaf: AccountLeaf,
        markets: Vec<MarketId>,
        levels: Vec<MarketId>,
    },
    Position(Position),
    Levels(PositionLevels),
    Market(MarketLeaf),
    Entitlement(TokenUnits),
    Distribution(UsdcUnits),
    Approval(WithdrawalApproval),
    Queued(QueuedWithdrawal),
    /// A pending deposit's principal, and its receipt when one backs it.
    Pending {
        reference: String,
        amount: Fixed,
        receipt: Option<PendingReceipt>,
    },
    /// A held deposit's principal, and its receipt when one binding its payer backs it.
    Held {
        reference: String,
        amount: Fixed,
        receipt: Option<PendingReceipt>,
    },
    /// An unfinalised credit.
    Credit {
        reference: String,
        receipt: PendingReceipt,
    },
    Terms(ExitTerms),
}

/// One leaf of a root: its key and value hash, `None` to remove it, and its value for proofs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafUpdate {
    pub(crate) key: B256,
    pub(crate) value_hash: Option<B256>,
    pub(crate) value: Option<Box<LeafValue>>,
}

/// The principal of the pending and of the held deposits a version holds, which its exit terms
/// commit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExitTotals {
    pub(crate) pending: Fixed,
    pub(crate) held: Fixed,
}

/// An exit leaf the index finds by the account it names: a queued request by its account, and a
/// receipt-backed pending deposit or unfinalised credit by its beneficiary, the house's under
/// `None`; and every receipt-backed held deposit.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Association {
    Queued(AccountId, u64),
    Pending(Option<AccountId>, String),
    Held(String),
    Credit(Option<AccountId>, String),
}

/// The exit's leaves by the account they name, in every version kept and the working one: a read
/// of one version keeps those its leaves still name.
#[derive(Debug, Default)]
pub(crate) struct ExitIndex {
    pub(crate) queued: BTreeMap<AccountId, BTreeSet<u64>>,
    pub(crate) pending: BTreeMap<Option<AccountId>, BTreeSet<String>>,
    pub(crate) held: BTreeSet<String>,
    pub(crate) credits: BTreeMap<Option<AccountId>, BTreeSet<String>>,
}

impl LeafUpdate {
    /// A write of the leaf at `key`, with no value for proofs.
    pub(crate) fn hashed(key: B256, value_hash: Option<B256>) -> Self {
        Self { key, value_hash, value: None }
    }

    /// A write of the leaf at `key` with `value` for proofs, or its removal for `None`.
    pub(crate) fn valued(key: B256, value: Option<(B256, LeafValue)>) -> Self {
        let (value_hash, value) = value.map(|(hash, value)| (hash, Box::new(value))).unzip();
        Self { key, value_hash, value }
    }
}

impl LeafValue {
    /// Whether the leaf is one of the exit's, whose totals or index a write moves.
    pub(crate) fn is_exit(&self) -> bool {
        matches!(self, Self::Queued(_) | Self::Pending { .. } | Self::Held { .. } | Self::Credit { .. })
    }

    /// The index entry the leaf makes, if it is an exit leaf the index finds.
    pub(crate) fn association(&self) -> Option<Association> {
        match self {
            Self::Queued(queued) => Some(Association::Queued(queued.account, queued.requested_seq)),
            Self::Pending { reference, receipt: Some(receipt), .. } => {
                Some(Association::Pending(receipt.account, reference.clone()))
            }
            Self::Held { reference, receipt: Some(_), .. } => Some(Association::Held(reference.clone())),
            Self::Credit { reference, receipt } => Some(Association::Credit(receipt.account, reference.clone())),
            _ => None,
        }
    }

    /// Moves `totals` from the principal of `old` to that of `new`, for a pending or held deposit's
    /// leaf.
    pub(crate) fn shift_totals(old: Option<&Self>, new: Option<&Self>, totals: &mut ExitTotals) {
        let principal = |value: Option<&Self>| match value {
            Some(Self::Pending { amount, .. }) => (*amount, Fixed::ZERO),
            Some(Self::Held { amount, .. }) => (Fixed::ZERO, *amount),
            _ => (Fixed::ZERO, Fixed::ZERO),
        };
        let ((old_pending, old_held), (new_pending, new_held)) = (principal(old), principal(new));
        totals.pending = totals.pending - old_pending + new_pending;
        totals.held = totals.held - old_held + new_held;
    }
}

impl Association {
    /// The key of the leaf the entry names.
    pub(crate) fn key(&self) -> B256 {
        match self {
            Self::Queued(_, seq) => withdrawal_queue_key(*seq),
            Self::Pending(_, reference) => pending_deposit_key(reference),
            Self::Held(reference) => held_deposit_key(reference),
            Self::Credit(_, reference) => unfinalised_credit_key(reference),
        }
    }
}

impl ExitIndex {
    /// Adds `entry` under its account.
    pub(crate) fn insert(&mut self, entry: Association) {
        match entry {
            Association::Queued(account, seq) => _ = self.queued.entry(account).or_default().insert(seq),
            Association::Pending(account, reference) => _ = self.pending.entry(account).or_default().insert(reference),
            Association::Held(reference) => _ = self.held.insert(reference),
            Association::Credit(account, reference) => _ = self.credits.entry(account).or_default().insert(reference),
        }
    }

    /// Removes `entry`, and its account's set once empty.
    pub(crate) fn remove(&mut self, entry: &Association) {
        match entry {
            Association::Queued(account, seq) => unindex(&mut self.queued, *account, seq),
            Association::Pending(account, reference) => unindex(&mut self.pending, *account, reference),
            Association::Held(reference) => _ = self.held.remove(reference),
            Association::Credit(account, reference) => unindex(&mut self.credits, *account, reference),
        }
    }
}

/// Removes `item` from `key`'s set in `index`, and the set once it is empty.
fn unindex<K: Ord, T: Ord>(index: &mut BTreeMap<K, BTreeSet<T>>, key: K, item: &T) {
    if let Some(items) = index.get_mut(&key) {
        items.remove(item);
        if items.is_empty() {
            index.remove(&key);
        }
    }
}
