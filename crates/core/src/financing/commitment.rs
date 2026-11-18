//! The commitment a `V80` rollover logs over each account's charges: a binary keccak Merkle tree whose
//! leaves are the charged accounts in ascending order. A leaf is
//! `keccak("engine.financing.v1" ‖ account ‖ n ‖ (market ‖ amount) × n)`, the account's `n` charges
//! in market order, `n` and each market as 4 big-endian bytes and each amount as `Fixed`'s 16
//! big-endian bytes; a node is `keccak("engine.financing.node.v1" ‖ left ‖ right)`; a level's odd last
//! node carries up unchanged. An account's proof is its leaf's index, the leaf count, and the siblings
//! from the leaf up, so anyone holding the logged root checks that account's rows without the others.

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::event::Event;
use crate::merkle::keccak;
use crate::types::{AccountId, MarketId};

const LEAF_TAG: &[u8] = b"engine.financing.v1";
const NODE_TAG: &[u8] = b"engine.financing.node.v1";

/// One position's financing at a rollover, positive when the account pays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Charge {
    pub market: MarketId,
    pub amount: Fixed,
}

/// One position's financing at a `V80` rollover as the command derives it (`Engine::take_derived`):
/// the `Event::Financing` the rollover logged before `V80`, in a fifth of its memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinancingRow {
    pub account: AccountId,
    pub market: MarketId,
    pub amount: Fixed,
}

/// One account's charges at one rollover, with the path from its leaf to the root the rollover's
/// `Event::FinancingApplied` logs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinancingProof {
    pub account: AccountId,
    /// The account's charges, in market order.
    pub charges: Vec<Charge>,
    /// The leaf's place among the charged accounts, in ascending order, and how many there are.
    pub index: u64,
    pub count: u64,
    /// The siblings from the leaf up; a level where the path's node is the odd last one has none.
    pub siblings: Vec<B256>,
}

/// One rollover's tree over its rows, which the rollover derives in ascending account order and,
/// within an account, in market order: the charged accounts in order, and every level from the leaves
/// up, so many accounts' proofs come from one pass over the rows.
#[derive(Clone, Debug)]
pub struct ChargesTree {
    accounts: Vec<AccountId>,
    levels: Vec<Vec<B256>>,
}

impl FinancingProof {
    /// Whether the account's charges lead to `root` along the path.
    pub fn verify(&self, root: B256) -> bool {
        if self.index >= self.count || self.charges.is_empty() {
            return false;
        }
        let (mut hash, mut index, mut size) = (leaf_hash(self.account, &self.charges), self.index, self.count);
        let mut siblings = self.siblings.iter();
        while size > 1 {
            if index % 2 == 1 {
                let Some(left) = siblings.next() else { return false };
                hash = node_hash(*left, hash);
            } else if index + 1 < size {
                let Some(right) = siblings.next() else { return false };
                hash = node_hash(hash, *right);
            }
            (index, size) = (index / 2, size.div_ceil(2));
        }
        siblings.next().is_none() && hash == root
    }
}

impl FinancingRow {
    /// The `Event::Financing` the row stands for.
    pub fn event(&self) -> Event {
        Event::Financing { account: self.account, market: self.market, amount: self.amount }
    }

    /// The row's charge to its account.
    pub fn charge(&self) -> Charge {
        Charge { market: self.market, amount: self.amount }
    }
}

impl ChargesTree {
    /// The tree over `rows`, one rollover's rows.
    pub fn new(rows: &[FinancingRow]) -> Self {
        let (mut accounts, mut leaves, mut charges) = (Vec::new(), Vec::new(), Vec::new());
        for run in rows.chunk_by(|left, right| left.account == right.account) {
            charges.clear();
            charges.extend(run.iter().map(FinancingRow::charge));
            accounts.push(run[0].account);
            leaves.push(leaf_hash(run[0].account, &charges));
        }
        let mut levels = vec![leaves];
        while levels.last().is_some_and(|level| level.len() > 1) {
            let above = level_above(levels.last().expect("a level"));
            levels.push(above);
        }
        Self { accounts, levels }
    }

    /// The root the rollover logs; zero over no rows.
    pub fn root(&self) -> B256 {
        self.levels.last().and_then(|top| top.first()).copied().unwrap_or(B256::ZERO)
    }

    /// `account`'s proof among `rows`, the rows the tree was built over, or `None` when it was charged
    /// nothing.
    pub fn proof(&self, rows: &[FinancingRow], account: AccountId) -> Option<FinancingProof> {
        let index = self.accounts.binary_search(&account).ok()?;
        let siblings = self
            .levels
            .iter()
            .take(self.levels.len() - 1)
            .enumerate()
            .filter_map(|(height, level)| {
                let at = index >> height;
                if at % 2 == 1 { Some(level[at - 1]) } else { level.get(at + 1).copied() }
            })
            .collect();
        let start = rows.partition_point(|row| row.account < account);
        let end = rows.partition_point(|row| row.account <= account);
        let charges = rows[start..end].iter().map(FinancingRow::charge).collect();
        let count = self.accounts.len() as u64;
        Some(FinancingProof { account, charges, index: index as u64, count, siblings })
    }
}

/// An account's leaf over its `charges`, in market order.
pub fn leaf_hash(account: AccountId, charges: &[Charge]) -> B256 {
    let mut bytes = Vec::with_capacity(LEAF_TAG.len() + 24 + charges.len() * 20);
    bytes.extend_from_slice(LEAF_TAG);
    bytes.extend_from_slice(&account.0);
    bytes.extend_from_slice(&count_bytes(charges.len()));
    for charge in charges {
        bytes.extend_from_slice(&charge.market.to_be_bytes());
        bytes.extend_from_slice(&charge.amount.to_be_bytes());
    }
    keccak(&[&bytes])
}

/// The node over two children.
pub fn node_hash(left: B256, right: B256) -> B256 {
    keccak(&[NODE_TAG, left.as_slice(), right.as_slice()])
}

/// The root over `leaves`, in order; zero for none.
pub fn root_of(mut leaves: Vec<B256>) -> B256 {
    while leaves.len() > 1 {
        leaves = level_above(&leaves);
    }
    leaves.first().copied().unwrap_or(B256::ZERO)
}

/// Each account's charges in one rollover's rows (`ChargesTree`), in order.
pub fn charges_of(rows: &[FinancingRow]) -> Vec<(AccountId, Vec<Charge>)> {
    rows.chunk_by(|left, right| left.account == right.account)
        .map(|run| (run[0].account, run.iter().map(FinancingRow::charge).collect()))
        .collect()
}

/// `account`'s proof among one rollover's rows (`ChargesTree`), or `None` when it was charged nothing.
pub fn proof(rows: &[FinancingRow], account: AccountId) -> Option<FinancingProof> {
    ChargesTree::new(rows).proof(rows, account)
}

/// The level above `nodes`: each pair's node, and an odd last node carried up.
fn level_above(nodes: &[B256]) -> Vec<B256> {
    nodes.chunks(2).map(|pair| if let [left, right] = pair { node_hash(*left, *right) } else { pair[0] }).collect()
}

/// A leaf's charge count as 4 big-endian bytes.
fn count_bytes(count: usize) -> [u8; 4] {
    u32::try_from(count).expect("an account holds a position in each of at most u32::MAX markets").to_be_bytes()
}
