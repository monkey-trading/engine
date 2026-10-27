//! Derived indexes over positions, triggers and custody receipt coverage, excluded from state.

use std::collections::{BTreeMap, BTreeSet};

use crate::Fixed;
use crate::access::{AccessDeposit, UncreditedReceipt};
use crate::account::RestingOrder;
use crate::command::OrderKind;
use crate::deposits::DepositStatus;
use crate::engine::Engine;
use crate::types::{AccountId, MarketId, OrderId, Side};

#[derive(Debug, Default)]
pub(crate) struct HotIndexes {
    pub(crate) holders: BTreeMap<MarketId, BTreeSet<AccountId>>,
    pub(crate) pending_deposits: BTreeSet<String>,
    /// Terminal dispositions by transaction-local identity, rebuilt from retained receipt history.
    pub(crate) terminal_deposit_holds: BTreeMap<String, UncreditedReceipt>,
    pub(crate) receipts_above_frontier: BTreeSet<String>,
    triggers: BTreeMap<MarketId, Triggers>,
    /// Pending deposit references by beneficiary; the house's are under `None`.
    pub(crate) pending: BTreeMap<Option<AccountId>, BTreeSet<String>>,
    /// Pending references a retry could settle: the house's and declared accounts'.
    pub(crate) retryable: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct Triggers {
    below: BTreeMap<(Fixed, OrderId), AccountId>,
    above: BTreeMap<(Fixed, OrderId), AccountId>,
}

impl HotIndexes {
    /// Retires only the newly covered block prefix; later finalised receipts still contribute.
    pub(crate) fn cover_deposits_through(&mut self, block: u64) {
        let next = block.checked_add(1).expect("custody frontier excludes the maximum block");
        self.receipts_above_frontier = self.receipts_above_frontier.split_off(&format!("{next:020}:"));
    }

    /// The triggered subset in the same time priority as the authoritative resting book.
    pub(crate) fn triggered(&self, market: MarketId, mid: Fixed) -> Vec<(OrderId, AccountId)> {
        let Some(book) = self.triggers.get(&market) else { return Vec::new() };
        let mut orders: Vec<_> = book
            .below
            .range(..=(mid, OrderId::MAX))
            .chain(book.above.range((mid, 0)..))
            .map(|((_, id), account)| (*id, *account))
            .collect();
        orders.sort_unstable_by_key(|(id, _)| *id);
        orders
    }

    fn order(&mut self, account: AccountId, order: &RestingOrder, insert: bool) {
        let (price, below) = match (order.kind, order.side) {
            (OrderKind::Limit { price }, Side::Sell) | (OrderKind::Stop { price }, Side::Buy) => (price, true),
            (OrderKind::Limit { price }, Side::Buy) | (OrderKind::Stop { price }, Side::Sell) => (price, false),
            (OrderKind::Market { .. }, _) => return,
        };
        let book = self.triggers.entry(order.market).or_default();
        let index = if below { &mut book.below } else { &mut book.above };
        if insert {
            index.insert((price, order.order_id), account);
        } else {
            index.remove(&(price, order.order_id));
        }
    }
}

impl Engine {
    /// Rebuilds the derived indexes a deserialized engine lacks, as its next command would, for a
    /// reader that uses restored state before applying anything.
    pub fn restore_indexes(&mut self) {
        self.ensure_hot_indexes();
    }

    /// Restores transient indexes from authoritative state once after construction or recovery.
    pub(crate) fn ensure_hot_indexes(&mut self) {
        if self.hot_indexes.is_some() {
            return;
        }
        let mut indexes = HotIndexes::default();
        indexes.pending_deposits.extend(self.deposits.iter().filter_map(|(reference, recorded)| {
            (recorded.status == DepositStatus::Unfinalised).then_some(reference.clone())
        }));
        indexes.receipts_above_frontier.extend(self.deposits.iter().filter_map(|(reference, recorded)| {
            (recorded.status != DepositStatus::Reversed
                && self.custody_checkpoint.is_none_or(|checkpoint| recorded.receipt.block > checkpoint.block))
            .then_some(reference.clone())
        }));
        for (_, recorded) in self.deposits.iter() {
            if let Some(held) = recorded.uncredited.filter(|held| held.reason.holds_deposit()) {
                indexes.terminal_deposit_holds.entry(recorded.receipt.transaction_reference()).or_insert(held);
            }
        }
        for (account, state) in self.accounts.iter() {
            for market in state.positions.keys() {
                indexes.holders.entry(*market).or_default().insert(*account);
            }
            for order in state.orders.values() {
                indexes.order(*account, order, true);
            }
        }
        for (reference, deposit) in self.pending_deposits.iter() {
            indexes.pending.entry(deposit.account).or_default().insert(reference.clone());
            if deposit.account.is_none_or(|account| self.eligibility.contains_key(&account)) {
                indexes.retryable.insert(reference.clone());
            }
        }
        self.hot_indexes = Some(indexes);
    }

    /// Records a reference entering (`insert`) or leaving the pending set, at the same boundary as
    /// the authoritative map.
    pub(crate) fn index_pending(&mut self, deposit: &AccessDeposit, insert: bool) {
        let declared = deposit.account.is_none_or(|account| self.eligibility.contains_key(&account));
        let Some(indexes) = self.hot_indexes.as_mut() else { return };
        let reference = &deposit.chain_ref;
        if insert {
            indexes.pending.entry(deposit.account).or_default().insert(reference.clone());
            if declared {
                indexes.retryable.insert(reference.clone());
            }
            return;
        }
        if let Some(references) = indexes.pending.get_mut(&deposit.account) {
            references.remove(reference);
            if references.is_empty() {
                indexes.pending.remove(&deposit.account);
            }
        }
        indexes.retryable.remove(reference);
    }

    /// Makes a newly declared account's pending deposits retryable.
    pub(crate) fn index_declaration(&mut self, account: AccountId) {
        let Some(indexes) = self.hot_indexes.as_mut() else { return };
        if let Some(references) = indexes.pending.get(&Some(account)) {
            indexes.retryable.extend(references.iter().cloned());
        }
    }

    /// Keeps membership aligned with the net position after every fill.
    pub(crate) fn index_position(&mut self, account: AccountId, market: MarketId) {
        let Some(indexes) = self.hot_indexes.as_mut() else { return };
        let holders = indexes.holders.entry(market).or_default();
        if self.accounts.get(&account).is_some_and(|state| state.positions.contains_key(&market)) {
            holders.insert(account);
        } else {
            holders.remove(&account);
        }
    }

    /// Updates a trigger at the same boundary as the authoritative resting order.
    pub(crate) fn index_order(&mut self, account: AccountId, order: &RestingOrder, insert: bool) {
        if let Some(indexes) = self.hot_indexes.as_mut() {
            indexes.order(account, order, insert);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Position;

    #[test]
    fn position_membership_tracks_open_flat_and_recovered_positions() {
        let (a, b) = (AccountId([1; 20]), AccountId([2; 20]));
        let mut engine = Engine::new();
        engine.ensure_hot_indexes();
        let position = Position { qty: Fixed::from_int(1), entry: Fixed::from_int(100) };
        engine.accounts.entry(a).or_default().positions.insert(1, position);
        engine.record_position(a, 1);
        engine.accounts.entry(b).or_default().positions.insert(2, position);
        engine.record_position(b, 2);
        assert_eq!(engine.hot_indexes.as_ref().unwrap().holders[&1], BTreeSet::from([a]));
        assert_eq!(engine.hot_indexes.as_ref().unwrap().holders[&2], BTreeSet::from([b]));
        engine.accounts.get_mut(&a).unwrap().positions.remove(&1);
        engine.record_position(a, 1);
        assert!(engine.hot_indexes.as_ref().unwrap().holders[&1].is_empty());

        let mut recovered: Engine = serde_json::from_slice(&serde_json::to_vec(&engine).unwrap()).unwrap();
        assert!(recovered.hot_indexes.is_none());
        recovered.ensure_hot_indexes();
        assert!(!recovered.hot_indexes.as_ref().unwrap().holders.contains_key(&1));
        assert_eq!(recovered.hot_indexes.as_ref().unwrap().holders[&2], BTreeSet::from([b]));
    }
}
