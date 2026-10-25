//! Derived indexes over positions, triggers and custody receipt coverage, excluded from state.

use std::collections::{BTreeMap, BTreeSet};

use crate::Fixed;
use crate::account::RestingOrder;
use crate::command::OrderKind;
use crate::deposits::DepositStatus;
use crate::engine::Engine;
use crate::types::{AccountId, MarketId, OrderId, Side};

#[derive(Debug, Default)]
pub(crate) struct HotIndexes {
    pub(crate) holders: BTreeMap<MarketId, BTreeSet<AccountId>>,
    pub(crate) pending_deposits: BTreeSet<String>,
    pub(crate) receipts_above_frontier: BTreeSet<String>,
    triggers: BTreeMap<MarketId, Triggers>,
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
        for (account, state) in self.accounts.iter() {
            for market in state.positions.keys() {
                indexes.holders.entry(*market).or_default().insert(*account);
            }
            for order in state.orders.values() {
                indexes.order(*account, order, true);
            }
        }
        self.hot_indexes = Some(indexes);
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
