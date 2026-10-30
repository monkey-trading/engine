//! What the vault's exit against the last posted root checks, from `V36`: the exit terms leaf, the
//! ABI leaves of receipt-backed pending deposits, an account proof's equity at the root's marks, and
//! the proofs an account exits with.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256};
use alloy_sol_types::SolValue;
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::access::{AccessDeposit, HeldDeposit};
use crate::deposits::RecordedDeposit;
use crate::engine::Engine;
use crate::fixed::Rounding;
use crate::merkle::{
    AccountProof, DistributionProof, QueuedWithdrawalProof, WithdrawalApprovalProof, json_hash, keccak,
    pending_deposit_key, verify, withdrawal_queue_key,
};
use crate::protocol::ProtocolVersion;
use crate::types::AccountId;
use crate::withdrawal::QueuedWithdrawal;

/// The kind byte of the exit terms leaf's key.
const EXIT_TERMS: u8 = 24;
/// A work bound: the exit terms name at most this many gaps below the cursor, the lowest, so the
/// vault starts the exit in bounded gas; the terms' `unheld` carries the rest's principal, which the
/// vault keeps rather than pays out.
pub const MAX_EXIT_GAPS: usize = 64;

/// What the vault's exit sets aside before it pays claims, as of a root.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitTerms {
    /// One past the highest vault deposit id the root holds, credited, pending, or held: the exit
    /// refunds every deposit from it on to its payer.
    pub deposit_cursor: u64,
    /// Principal of the pending deposits, which the exit refunds to their payers.
    pub pending: Fixed,
    /// Principal of the held deposits, which the exit never pays.
    pub held: Fixed,
    /// Principal of the vault deposits below the cursor the root does not hold, the named gaps
    /// included: the exit reserves it, refunds the named gaps by id, and keeps the rest, which only
    /// a reorganisation deeper than [`MAX_EXIT_GAPS`] deposits leaves unnamed.
    pub unheld: Fixed,
    /// The vault deposit ids below the cursor the root does not hold, the lowest
    /// [`MAX_EXIT_GAPS`], in increasing order: empty unless a reorganisation the custody adapter is
    /// still applying left them. The exit refunds them too.
    pub gaps: Vec<u64>,
}

/// The exit terms leaf with its proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitTermsProof {
    pub seq: u64,
    pub root: B256,
    pub terms: ExitTerms,
    pub siblings: Vec<B256>,
}

/// A receipt-backed pending deposit as its `V36` leaf holds it, with its proof, as the vault's
/// exit refunds it to its payer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingDepositProof {
    pub seq: u64,
    pub root: B256,
    /// The receipt's reference, whose UTF-8 bytes key the leaf.
    pub chain_ref: String,
    pub deposit: PendingReceipt,
    pub siblings: Vec<B256>,
}

/// What a receipt-backed pending deposit's `V36` leaf binds: the vault's deposit id, the
/// beneficiary (`None` for the house), the funding address, and the principal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingReceipt {
    pub deposit_id: u64,
    pub account: Option<AccountId>,
    pub payer: Option<AccountId>,
    pub amount: Fixed,
}

/// Every proof an account exits with against one root: the exit terms, the account with its
/// positions and their markets, each approval it holds, each of its queued requests, its
/// distribution, and the pending deposits for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitProof {
    pub seq: u64,
    pub root: B256,
    pub terms: ExitTermsProof,
    pub account: Option<AccountProof>,
    /// Every approval the account holds; the vault's `usedNonces` says which are unpaid.
    pub approvals: Vec<WithdrawalApprovalProof>,
    pub queued: Vec<QueuedWithdrawalProof>,
    pub distribution: Option<DistributionProof>,
    pub pending_deposits: Vec<PendingDepositProof>,
}

/// The leaves the exit proves, as of the last root: committed copies of the queue and of the
/// pending and held deposits, so proofs and totals follow the root rather than the live state.
#[derive(Debug, Default)]
pub(crate) struct ExitLeaves {
    queued: BTreeMap<u64, QueuedWithdrawal>,
    /// The committed queue's request seqs, by account.
    queued_by_account: BTreeMap<AccountId, BTreeSet<u64>>,
    pending: BTreeMap<String, (Fixed, Option<PendingReceipt>)>,
    /// The committed receipt-backed pending references, by beneficiary; the house's under `None`.
    pending_by_account: BTreeMap<Option<AccountId>, BTreeSet<String>>,
    held: BTreeMap<String, Fixed>,
    pending_total: Fixed,
    held_total: Fixed,
    terms: Option<ExitTerms>,
}

impl ExitTermsProof {
    /// Whether the terms prove against `root`, which the caller takes from a posted checkpoint.
    pub fn verify(&self, root: B256) -> bool {
        verify(root, exit_terms_key(), exit_terms_hash(&self.terms), &self.siblings)
    }
}

impl PendingDepositProof {
    /// Whether the pending receipt proves against `root`, which the caller takes from a posted
    /// checkpoint.
    pub fn verify(&self, root: B256) -> bool {
        verify(root, pending_deposit_key(&self.chain_ref), pending_receipt_hash(&self.deposit), &self.siblings)
    }
}

impl AccountProof {
    /// The account's equity at the root's marks, as `Engine::equity_and_margin` values it from
    /// `V5`: its balance plus each position's price PnL at its market's mark (at its entry before
    /// the first mark), rounded down, in USD at the market's FX mark, rounded down. `None` when a
    /// market quoted in another currency has PnL and no FX mark, which no engine state holds.
    pub fn equity(&self) -> Option<Fixed> {
        self.positions.iter().zip(&self.markets).try_fold(self.leaf.balance, |equity, (held, market)| {
            let mark = market.leaf.mark.unwrap_or(held.position.entry);
            let pnl = held.position.unrealised(mark, ProtocolVersion::V36);
            let currency = market.leaf.quote_currency;
            let usd = match market.leaf.fx_mark {
                _ if pnl.is_zero() || currency.fx_market().is_none() => pnl,
                Some(fx) => currency.to_usd(pnl, Some(fx), Rounding::Down),
                None => return None,
            };
            Some(equity + usd)
        })
    }
}

impl ExitLeaves {
    /// Brings the committed queue up to `queue` at `changed`, the request seqs changed since the
    /// last root.
    pub(super) fn update_queue(&mut self, queue: &BTreeMap<u64, QueuedWithdrawal>, changed: &BTreeSet<u64>) {
        for seq in changed {
            let old = match queue.get(seq) {
                Some(queued) => self.queued.insert(*seq, *queued),
                None => self.queued.remove(seq),
            };
            if let Some(old) = old {
                unindex(&mut self.queued_by_account, old.account, seq);
            }
            if let Some(queued) = queue.get(seq) {
                self.queued_by_account.entry(queued.account).or_default().insert(*seq);
            }
        }
    }

    /// Brings the committed pending deposits and their total up to `pending` at `changed`, the
    /// references changed since the last root, with each one's vault deposit id from `receipts`.
    pub(super) fn update_pending(
        &mut self,
        pending: &BTreeMap<String, AccessDeposit>,
        receipts: &BTreeMap<String, RecordedDeposit>,
        changed: &BTreeSet<String>,
    ) {
        for reference in changed {
            let new = pending.get(reference).map(|deposit| {
                let receipt = receipts.get(reference).map(|recorded| PendingReceipt {
                    deposit_id: recorded.receipt.deposit_id,
                    account: deposit.account,
                    payer: deposit.payer,
                    amount: deposit.amount,
                });
                (deposit.amount, receipt)
            });
            let old = match new {
                Some(entry) => self.pending.insert(reference.clone(), entry),
                None => self.pending.remove(reference),
            };
            if let Some((_, Some(receipt))) = old {
                unindex(&mut self.pending_by_account, receipt.account, reference);
            }
            if let Some((_, Some(receipt))) = new {
                self.pending_by_account.entry(receipt.account).or_default().insert(reference.clone());
            }
            let amount = |entry: Option<(Fixed, _)>| entry.map_or(Fixed::ZERO, |(amount, _)| amount);
            self.pending_total = self.pending_total - amount(old) + amount(new);
        }
    }

    /// Brings the committed held deposits and their total up to `held` at `changed`, the references
    /// changed since the last root.
    pub(super) fn update_held(&mut self, held: &BTreeMap<String, HeldDeposit>, changed: &BTreeSet<String>) {
        for reference in changed {
            let new = held.get(reference).map(|hold| hold.deposit.amount);
            let old = match new {
                Some(amount) => self.held.insert(reference.clone(), amount),
                None => self.held.remove(reference),
            };
            self.held_total = self.held_total - old.unwrap_or(Fixed::ZERO) + new.unwrap_or(Fixed::ZERO);
        }
    }

    /// Records the terms the root holds, from the deposit cursor, the gaps below it, and the
    /// unheld principal, and the committed totals, and returns their leaf's value hash.
    pub(super) fn commit_terms(&mut self, (deposit_cursor, gaps, unheld): (u64, Vec<u64>, Fixed)) -> B256 {
        let terms = ExitTerms { deposit_cursor, pending: self.pending_total, held: self.held_total, unheld, gaps };
        let hash = exit_terms_hash(&terms);
        self.terms = Some(terms);
        hash
    }

    /// The committed receipt-backed pending deposits for `beneficiary`, `None` for the house.
    fn pending_for(&self, beneficiary: Option<AccountId>) -> impl Iterator<Item = (&String, PendingReceipt)> {
        self.pending_by_account.get(&beneficiary).into_iter().flatten().map(|reference| {
            let (_, receipt) = &self.pending[reference];
            (reference, receipt.expect("only receipt-backed pending deposits are indexed"))
        })
    }

    /// The value hash of the pending leaf at `reference`, as the last update committed it: from
    /// `V36` a receipt-backed deposit's ABI leaf, and otherwise the keccak-256 of `deposit`'s JSON.
    pub(super) fn pending_hash(&self, reference: &str, deposit: &AccessDeposit, version: ProtocolVersion) -> B256 {
        match self.pending.get(reference) {
            Some((_, Some(receipt))) if version >= ProtocolVersion::V36 => pending_receipt_hash(receipt),
            _ => json_hash(deposit),
        }
    }
}

impl Engine {
    /// The exit's deposit cursor, one past the highest vault deposit id the engine holds a live
    /// receipt for, the gaps below it, the lowest [`MAX_EXIT_GAPS`], and the principal of every id
    /// below the cursor no live receipt holds. Every id below the finalised frontier's count has a
    /// settled receipt, since the custody adapter reads deposits in id order with no gap
    /// (`custody::evm::check_complete`); above it, a replacement sequenced before the deposit that
    /// took its old id can leave one. Without a gap this reads the index's ends only; with one, it
    /// walks the missing runs, not the held receipts, and stops at [`MAX_EXIT_GAPS`].
    pub(super) fn exit_deposit_frontier(&mut self) -> (u64, Vec<u64>, Fixed) {
        self.ensure_hot_indexes();
        let indexes = self.hot_indexes.as_ref().expect("indexes initialised");
        let base = self.custody_checkpoint.map_or(0, |checkpoint| checkpoint.deposit_count);
        let cursor =
            indexes.above_frontier_by_id.last_key_value().map_or(base, |(id, _)| base.max(id.saturating_add(1)));
        let mut gaps = Vec::new();
        for (&start, &end) in indexes.missing.range(base..cursor) {
            for id in start..=end {
                if gaps.len() == MAX_EXIT_GAPS {
                    break;
                }
                gaps.push(id);
            }
            if gaps.len() == MAX_EXIT_GAPS {
                break;
            }
        }
        // The unheld principal covers the ids below the cursor only: at or past it every deposit is
        // refunded by id, and below the base every deposit is settled.
        let outside = indexes
            .unheld_amounts
            .range(..base)
            .chain(indexes.unheld_amounts.range(cursor..))
            .map(|(_, amount)| *amount)
            .sum::<Fixed>();
        (cursor, gaps, indexes.unheld_total - outside)
    }

    /// Every proof `account` exits with against the last root, or `None` before the first root
    /// under `V36`. The work is bounded by what the account itself holds: one approval for each of
    /// its withdrawal nonces, its queued requests, and the pending deposits for it.
    pub fn exit_proof(&self, account: AccountId) -> Option<ExitProof> {
        let state = self.tree.as_ref()?;
        let exit = &state.exit;
        exit.terms.as_ref()?;
        let (seq, root) = (state.seq, state.root);
        let proof = |key| state.tree.proof(key).expect("every leaf kept for exit proofs is in the tree");
        let account_proof = self.account_proof(account);
        let nonces = account_proof.as_ref().map_or(0, |proof| proof.leaf.withdrawal_nonce);
        let approvals = (1..=nonces).filter_map(|nonce| self.approval_proof(account, nonce)).collect();
        let queued = exit
            .queued_by_account
            .get(&account)
            .into_iter()
            .flatten()
            .map(|requested| QueuedWithdrawalProof {
                seq,
                root,
                queued: exit.queued[requested],
                siblings: proof(withdrawal_queue_key(*requested)),
            })
            .collect();
        let pending_deposits = exit
            .pending_for(Some(account))
            .map(|(reference, deposit)| PendingDepositProof {
                seq,
                root,
                chain_ref: reference.clone(),
                deposit,
                siblings: proof(pending_deposit_key(reference)),
            })
            .collect();
        Some(ExitProof {
            seq,
            root,
            terms: self.exit_terms_proof()?,
            account: account_proof,
            approvals,
            queued,
            distribution: self.distribution_proof(account),
            pending_deposits,
        })
    }

    /// The proof of the exit terms against the last root, or `None` before the first root under
    /// `V36`.
    pub fn exit_terms_proof(&self) -> Option<ExitTermsProof> {
        let state = self.tree.as_ref()?;
        let terms = state.exit.terms.clone()?;
        let siblings = state.tree.proof(exit_terms_key()).expect("the exit terms leaf is in every V36 tree");
        Some(ExitTermsProof { seq: state.seq, root: state.root, terms, siblings })
    }

    /// The proofs of the house's pending deposits against the last root, which the exit refunds to
    /// their payers, or `None` before the first root under `V36`.
    pub fn house_pending_deposit_proofs(&self) -> Option<Vec<PendingDepositProof>> {
        let state = self.tree.as_ref()?;
        state.exit.terms.as_ref()?;
        let proofs = state
            .exit
            .pending_for(None)
            .map(|(reference, deposit)| PendingDepositProof {
                seq: state.seq,
                root: state.root,
                chain_ref: reference.clone(),
                deposit,
                siblings: state.tree.proof(pending_deposit_key(reference)).expect("a committed pending leaf"),
            })
            .collect();
        Some(proofs)
    }
}

/// The exit terms leaf's key, `keccak256(abi.encodePacked(uint8(24)))`.
pub fn exit_terms_key() -> B256 {
    keccak(&[&[EXIT_TERMS]])
}

/// `keccak256(abi.encode(uint64 depositCursor, int128 pending, int128 held, uint64[] gaps))`.
pub fn exit_terms_hash(terms: &ExitTerms) -> B256 {
    let ExitTerms { deposit_cursor, pending, held, unheld, gaps } = terms;
    keccak(&[&(*deposit_cursor, pending.raw(), held.raw(), unheld.raw(), gaps.clone()).abi_encode_params()])
}

/// `keccak256(abi.encode(uint64 depositId, address account, address payer, int128 amount))`, with
/// the zero address for the house and for no payer.
pub fn pending_receipt_hash(deposit: &PendingReceipt) -> B256 {
    let address = |id: Option<AccountId>| id.map_or(Address::ZERO, |id| Address::from(id.0));
    keccak(
        &[&(deposit.deposit_id, address(deposit.account), address(deposit.payer), deposit.amount.raw()).abi_encode()],
    )
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
