//! The engine's state tree: which leaves each root rehashes, the leaf of the engine's remaining
//! state, and account and entitlement proofs.

use std::collections::BTreeMap;

use alloy_primitives::B256;
use serde::Serialize;
use sha3::{Digest, Keccak256};

mod touched;
use crate::Fixed;
use crate::account::Position;
use crate::auth::{ClientAuth, Operator};
use crate::benchmark::BenchmarkHistory;
use crate::deposits::CustodyCheckpoint;
use crate::emissions::{Emissions, TokenUnits};
use crate::engine::{Engine, Markout};
use crate::merkle::leaf::{deposit_key, deposit_transaction_key, legacy_deposit_key};
use crate::merkle::{
    AccountLeaf, AccountProof, DistributionProof, EntitlementProof, MarketLeaf, MarketProof, PositionProof,
    SparseMerkleTree, WithdrawalApprovalProof, account_key, corporate_action_key, corporate_reference_key,
    credited_ref_hash, credited_ref_key, distribution_hash, distribution_key, entitlement_hash, entitlement_key,
    globals_key, json_hash, market_key, markout_hash, markout_key, order_hash, order_key, period_result_hash,
    period_result_key, position_hash, position_key, request_nonce_hash, request_nonce_key, staked_hash, staked_key,
    withdrawal_approval_hash, withdrawal_approval_key, withdrawal_queue_hash, withdrawal_queue_key,
};
use crate::params::ProtocolParams;
use crate::protocol::ProtocolVersion;
use crate::staking::{Staking, UsdcUnits};
use crate::types::{AccountId, MarketId, Millis, OrderId};
pub(crate) use touched::Changed;
pub use touched::RootWork;
use touched::Touched;

/// The state tree as of the last root, with the account, position, market, and entitlement leaves
/// proofs need.
#[derive(Debug, Default)]
pub(crate) struct StateTree {
    tree: SparseMerkleTree,
    root: B256,
    seq: u64,
    accounts: BTreeMap<AccountId, AccountLeaf>,
    positions: BTreeMap<(AccountId, MarketId), Position>,
    markets: BTreeMap<MarketId, MarketLeaf>,
    entitlements: BTreeMap<AccountId, TokenUnits>,
    distributions: BTreeMap<AccountId, UsdcUnits>,
    work: RootWork,
}

/// The engine fields no leaf of their own holds.
#[derive(Serialize)]
struct Globals<'a> {
    params: &'a Option<ProtocolParams>,
    house_cash: Fixed,
    benchmarks: &'a BTreeMap<String, i64>,
    global_stress_floor: usize,
    next_order_id: OrderId,
    next_rollover: Option<Millis>,
    last_seq: Option<u64>,
    now: Millis,
    protocol_version: ProtocolVersion,
    client_auth: &'a Option<ClientAuth>,
    operator: &'a Option<Operator>,
    global_reduce_only: bool,
    markout_base: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    benchmark_history: &'a BenchmarkHistory,
    #[serde(skip_serializing_if = "Option::is_none")]
    emissions: &'a Option<Emissions>,
    #[serde(skip_serializing_if = "Fixed::is_zero")]
    unfinalised_house_deposits: Fixed,
    #[serde(skip_serializing_if = "Option::is_none")]
    custody_checkpoint: Option<CustodyCheckpoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    withdrawal_cash: Option<Fixed>,
    #[serde(skip_serializing_if = "is_zero")]
    withdrawal_next_ticket: u64,
    /// From `V25`; omitted until staking is initialised, so earlier roots stand.
    #[serde(skip_serializing_if = "Option::is_none")]
    staking: &'a Option<Staking>,
}

impl Engine {
    /// The keccak-256 commitment to the whole state, posted on-chain at checkpoints; anyone
    /// replaying the log must get the same root. From `V9`, the root of the state tree (`merkle`),
    /// which rehashes only what changed since the last root; before, the hash of the canonical state
    /// encoding.
    pub fn state_root(&mut self) -> [u8; 32] {
        if self.protocol_version >= ProtocolVersion::V9 {
            return self.merkle_root().0;
        }
        self.clear_touched();
        // Every field is a map, list, string, or integer, all of which serde_json encodes.
        let bytes = serde_json::to_vec(self).expect("engine state serialises");
        Keccak256::digest(bytes).into()
    }

    /// The root of the state tree, rehashing the leaves changed since the last root. With no tree
    /// yet, as on `V9`'s first root, every leaf is hashed.
    pub(crate) fn merkle_root(&mut self) -> B256 {
        let touched = self.take_touched();
        let Touched {
            accounts,
            markets,
            nonces,
            refs,
            period_results,
            entitlements,
            staked,
            distributions,
            deposits,
            deposit_transactions,
            legacy_deposits,
            withdrawal_queue,
            withdrawal_approvals,
            corporate_actions,
            corporate_refs,
            changed: Changed { markouts, positions, orders },
        } = match &self.tree {
            Some(_) => touched,
            None => self.every_key(),
        };
        let mut changes = BTreeMap::from([(globals_key(), Some(json_hash(&self.globals())))]);
        changes.extend(
            corporate_actions
                .iter()
                .map(|id| (corporate_action_key(id), self.corporate_actions.get(id).map(json_hash))),
        );
        changes.extend(
            corporate_refs
                .iter()
                .map(|id| (corporate_reference_key(id), self.corporate_refs.contains(id).then(|| json_hash(id)))),
        );
        let account_leaves: Vec<(AccountId, Option<AccountLeaf>)> =
            accounts.into_iter().map(|id| (id, self.accounts.get(&id).map(AccountLeaf::of))).collect();
        let market_leaves: Vec<(MarketId, Option<MarketLeaf>)> =
            markets.into_iter().map(|id| (id, self.markets.get(&id).map(MarketLeaf::of))).collect();
        changes.extend(
            account_leaves.iter().map(|(id, leaf)| (account_key(*id), leaf.as_ref().map(AccountLeaf::value_hash))),
        );
        changes.extend(
            market_leaves.iter().map(|(id, leaf)| (market_key(*id), leaf.as_ref().map(MarketLeaf::value_hash))),
        );
        changes.extend(
            nonces.into_iter().map(|id| {
                (request_nonce_key(id), self.request_nonces.get(&id).map(|nonce| request_nonce_hash(*nonce)))
            }),
        );
        changes.extend(
            refs.iter().map(|r| (credited_ref_key(r), self.credited_refs.contains(r).then(|| credited_ref_hash(r)))),
        );
        changes.extend(
            deposits.iter().map(|reference| (deposit_key(reference), self.deposits.get(reference).map(json_hash))),
        );
        changes.extend(deposit_transactions.iter().map(|reference| {
            (deposit_transaction_key(reference), self.deposit_transactions.get(reference).map(json_hash))
        }));
        changes.extend(
            legacy_deposits
                .iter()
                .map(|reference| (legacy_deposit_key(reference), self.legacy_deposits.get(reference).map(json_hash))),
        );
        changes.extend(markouts.into_iter().map(|index| (markout_key(index), self.markout(index).map(markout_hash))));
        let position_leaves: Vec<((AccountId, MarketId), Option<Position>)> = positions
            .into_iter()
            .map(|(id, market)| {
                ((id, market), self.accounts.get(&id).and_then(|acct| acct.positions.get(&market)).copied())
            })
            .collect();
        changes.extend(
            position_leaves
                .iter()
                .map(|((id, market), pos)| (position_key(*id, *market), pos.as_ref().map(position_hash))),
        );
        changes.extend(orders.into_iter().map(|(id, order_id)| {
            let order = self.accounts.get(&id).and_then(|acct| acct.orders.get(&order_id));
            (order_key(order_id), order.map(|order| order_hash(id, order)))
        }));
        changes.extend(
            period_results
                .into_iter()
                .map(|id| (period_result_key(id), self.period_results.get(&id).map(period_result_hash))),
        );
        let entitlement_leaves: Vec<(AccountId, Option<TokenUnits>)> =
            entitlements.into_iter().map(|id| (id, self.entitlements.get(&id).copied())).collect();
        changes.extend(
            entitlement_leaves.iter().map(|(id, entitled)| (entitlement_key(*id), entitled.map(entitlement_hash))),
        );
        let distribution_leaves: Vec<(AccountId, Option<UsdcUnits>)> =
            distributions.into_iter().map(|id| (id, self.distributions.get(&id).copied())).collect();
        changes.extend(
            distribution_leaves.iter().map(|(id, entitled)| (distribution_key(*id), entitled.map(distribution_hash))),
        );
        changes.extend(
            withdrawal_queue
                .into_iter()
                .map(|seq| (withdrawal_queue_key(seq), self.withdrawal_queue.get(&seq).map(withdrawal_queue_hash))),
        );
        changes.extend(
            withdrawal_approvals
                .into_iter()
                .map(|key| (key, self.withdrawal_approvals.get(&key).map(withdrawal_approval_hash))),
        );
        changes.extend(staked.into_iter().map(|id| (staked_key(id), self.staked.get(&id).map(|s| staked_hash(*s)))));

        // A seq has been applied: `V9` takes effect through a sequenced `ActivateProtocol`.
        let seq = self.last_seq.expect("a state root under V9 follows a command");
        let state = self.tree.get_or_insert_with(StateTree::default);
        let leaves = changes.len();
        let nodes = state.tree.update(changes);
        for (id, leaf) in account_leaves {
            match leaf {
                Some(leaf) => state.accounts.insert(id, leaf),
                None => state.accounts.remove(&id),
            };
        }
        for (id, leaf) in market_leaves {
            match leaf {
                Some(leaf) => state.markets.insert(id, leaf),
                None => state.markets.remove(&id),
            };
        }
        for (id, pos) in position_leaves {
            match pos {
                Some(pos) => state.positions.insert(id, pos),
                None => state.positions.remove(&id),
            };
        }
        for (id, entitled) in entitlement_leaves {
            match entitled {
                Some(entitled) => state.entitlements.insert(id, entitled),
                None => state.entitlements.remove(&id),
            };
        }
        for (id, entitled) in distribution_leaves {
            match entitled {
                Some(entitled) => state.distributions.insert(id, entitled),
                None => state.distributions.remove(&id),
            };
        }
        state.root = state.tree.root();
        state.seq = seq;
        state.work = RootWork { leaves, nodes };
        state.root
    }

    /// Clears the record of what changed since the last root, for a root that does not use it.
    pub(crate) fn clear_touched(&mut self) {
        self.take_touched();
    }

    /// The proof of `account`'s leaf, of each of its position leaves, and of the leaf of each
    /// market it holds a position in, against the last state root computed. `None` before the
    /// first root under `V9`, or for an account that root does not hold.
    pub fn account_proof(&self, account: AccountId) -> Option<AccountProof> {
        let state = self.tree.as_ref()?;
        let leaf = state.accounts.get(&account)?.clone();
        let proof = |key| state.tree.proof(key).expect("every leaf kept for proofs is in the tree");
        let held = state.positions.range((account, MarketId::MIN)..=(account, MarketId::MAX));
        let (positions, markets) = held
            .map(|((_, market), position)| {
                let siblings = proof(position_key(account, *market));
                // Positions are only ever opened in listed markets, and listings are never removed.
                let market_leaf = state.markets.get(market).expect("a position's market has a leaf").clone();
                (
                    PositionProof { market: *market, position: *position, siblings },
                    MarketProof { market: *market, leaf: market_leaf, siblings: proof(market_key(*market)) },
                )
            })
            .unzip();
        let siblings = proof(account_key(account));
        Some(AccountProof { seq: state.seq, root: state.root, account, leaf, siblings, positions, markets })
    }

    /// The proof of `account`'s cumulative MONKEY entitlement against the last state root computed.
    /// `None` before the first root under `V9`, or for an account that root holds no entitlement for.
    pub fn entitlement_proof(&self, account: AccountId) -> Option<EntitlementProof> {
        let state = self.tree.as_ref()?;
        let entitled = *state.entitlements.get(&account)?;
        let siblings =
            state.tree.proof(entitlement_key(account)).expect("every entitlement kept for proofs is in the tree");
        Some(EntitlementProof { seq: state.seq, root: state.root, account, entitled, siblings })
    }

    /// Proves the exact immutable payout against the last computed root, or `None` until included.
    pub fn approval_proof(&self, account: AccountId, nonce: u64) -> Option<WithdrawalApprovalProof> {
        let state = self.tree.as_ref()?;
        let approval = *self.withdrawal_approval(account, nonce)?;
        let siblings = state.tree.proof(withdrawal_approval_key(account, nonce))?;
        Some(WithdrawalApprovalProof { seq: state.seq, root: state.root, approval, siblings })
    }

    /// The proof of `account`'s cumulative USDC distribution against the last state root computed,
    /// as the vault's `claimDistribution` takes it. `None` before the first root under `V9`, or for
    /// an account that root holds no distribution for.
    pub fn distribution_proof(&self, account: AccountId) -> Option<DistributionProof> {
        let state = self.tree.as_ref()?;
        let entitled = *state.distributions.get(&account)?;
        let siblings =
            state.tree.proof(distribution_key(account)).expect("every distribution kept for proofs is in the tree");
        Some(DistributionProof { seq: state.seq, root: state.root, account, entitled, siblings })
    }

    /// What the last state root hashed, or `None` before the first root under `V9`.
    pub fn last_root_work(&self) -> Option<RootWork> {
        self.tree.as_ref().map(|state| state.work)
    }

    /// Records that `account`'s position in `market` changed, for the state root.
    pub(crate) fn record_position(&mut self, account: AccountId, market: MarketId) {
        self.index_position(account, market);
        self.changed.positions.insert((account, market));
    }

    /// Records that `account`'s resting order `order_id` was added or removed, for the state root.
    pub(crate) fn record_order(&mut self, account: AccountId, order_id: OrderId) {
        self.changed.orders.insert((account, order_id));
    }

    /// Records a split-normalized pending markout for its committed leaf.
    pub(crate) fn record_markout_change(&mut self, offset: usize) {
        self.changed.markouts.insert(self.markout_base + offset as u64);
    }

    /// Queues a markout, recording its index for the state root.
    pub(crate) fn queue_markout(&mut self, markout: Markout) {
        self.markouts.push_back(markout);
        self.changed.markouts.insert(self.markout_base + self.markouts.len() as u64 - 1);
    }

    /// Queues a markout in due order, after every markout due no later, recording for the state root
    /// the index of each markout it moves.
    pub(crate) fn insert_markout(&mut self, markout: Markout) {
        let at = self.markouts.partition_point(|queued| queued.due <= markout.due);
        self.markouts.insert(at, markout);
        let base = self.markout_base;
        self.changed.markouts.extend((at..self.markouts.len()).map(|position| base + position as u64));
    }

    /// Puts back at the front of the queue a markout `pop_markout` took in the same pass, recording
    /// its index for the state root.
    pub(crate) fn requeue_markout(&mut self, markout: Markout) {
        self.markouts.push_front(markout);
        if self.protocol_version >= ProtocolVersion::V9 {
            // `pop_markout` moved the index up when it took this markout.
            self.markout_base = self.markout_base.checked_sub(1).expect("a requeued markout was popped first");
        }
        self.changed.markouts.insert(self.markout_base);
    }

    /// Takes the markout at the front of the queue, recording its index for the state root. From
    /// `V9` the next markout's index moves up by one; earlier versions keep every index at 0, which
    /// their encoding leaves out.
    pub(crate) fn pop_markout(&mut self) -> Option<Markout> {
        let markout = self.markouts.pop_front()?;
        self.changed.markouts.insert(self.markout_base);
        if self.protocol_version >= ProtocolVersion::V9 {
            self.markout_base += 1;
        }
        Some(markout)
    }

    /// The queued markout at `index`, if it is still queued.
    fn markout(&self, index: u64) -> Option<&Markout> {
        let position = index.checked_sub(self.markout_base)?;
        self.markouts.get(usize::try_from(position).ok()?)
    }

    /// The keys of everything changed since the last root, clearing the record.
    fn take_touched(&mut self) -> Touched {
        Touched {
            accounts: self.accounts.take_touched(),
            markets: self.markets.take_touched(),
            nonces: self.request_nonces.take_touched(),
            refs: self.credited_refs.take_touched(),
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
            changed: std::mem::take(&mut self.changed),
        }
    }

    /// The key of every entry, for a root with no tree to update.
    fn every_key(&self) -> Touched {
        let accounts = || self.accounts.iter();
        Touched {
            accounts: self.accounts.keys().copied().collect(),
            markets: self.markets.keys().copied().collect(),
            nonces: self.request_nonces.keys().copied().collect(),
            refs: self.credited_refs.iter().cloned().collect(),
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
            changed: Changed {
                markouts: (self.markout_base..).take(self.markouts.len()).collect(),
                positions: accounts()
                    .flat_map(|(id, acct)| acct.positions.keys().map(|market| (*id, *market)))
                    .collect(),
                orders: accounts().flat_map(|(id, acct)| acct.orders.keys().map(|order_id| (*id, *order_id))).collect(),
            },
        }
    }

    /// The fields the remaining-state leaf holds.
    fn globals(&self) -> Globals<'_> {
        // Every field is named, so a field added to `Engine` fails to compile until a leaf holds it.
        let Engine {
            params,
            markets: _,
            accounts: _,
            house_cash,
            benchmarks,
            credited_refs: _,
            deposits: _,
            deposit_transactions: _,
            legacy_deposits: _,
            custody_checkpoint,
            unfinalised_house_deposits,
            global_stress_floor,
            next_order_id,
            next_rollover,
            markouts: _,
            last_seq,
            now,
            protocol_version,
            client_auth,
            request_nonces: _,
            operator,
            global_reduce_only,
            markout_base,
            benchmark_history,
            emissions,
            period_results: _,
            entitlements: _,
            withdrawal_cash,
            withdrawal_queue: _,
            withdrawal_next_ticket,
            withdrawal_approvals: _,
            corporate_actions: _,
            corporate_refs: _,
            staking,
            staked: _,
            distributions: _,
            changed: _,
            tree: _,
            hot_indexes: _,
        } = self;
        Globals {
            params,
            house_cash: *house_cash,
            benchmarks,
            global_stress_floor: *global_stress_floor,
            next_order_id: *next_order_id,
            next_rollover: *next_rollover,
            last_seq: *last_seq,
            now: *now,
            protocol_version: *protocol_version,
            client_auth,
            operator,
            global_reduce_only: *global_reduce_only,
            markout_base: *markout_base,
            benchmark_history,
            emissions,
            unfinalised_house_deposits: *unfinalised_house_deposits,
            custody_checkpoint: *custody_checkpoint,
            withdrawal_cash: *withdrawal_cash,
            withdrawal_next_ticket: *withdrawal_next_ticket,
            staking,
        }
    }
}

/// Whether a persisted index is zero, so its default stays out of earlier state encodings.
pub(crate) fn is_zero(index: &u64) -> bool {
    *index == 0
}
