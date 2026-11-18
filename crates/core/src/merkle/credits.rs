//! From `V77`, the unfinalised credit leaves: one per credited receipt whose principal still awaits
//! finality, the house's included, binding what the vault's own deposit records at its id must match:
//! its payer, amount and beneficiary. The vault's exit leaves an account's unfinalised principal out
//! of its claim and adds back, credit by credit, each one its records match, and refunds the deposit
//! at an id whose credited, pending or held receipt they do not match, so a receipt a reorganisation
//! orphaned or moved is never paid out of other clients' funds. Each credit's principal is locked in
//! its account's unfinalised principal from credit to settlement (`deposits::cash`), so an account's
//! credits sum to the unfinalised principal its account leaf commits, but for legacy principal no
//! receipt has adopted, which only a log migrated from before `V20` holds and which no credit adds
//! back.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::deposits::{DepositBeneficiary, DepositStatus, RecordedDeposit};
use crate::engine::Engine;
use crate::merkle::exit::unindex;
use crate::merkle::leaf::{HELD_DEPOSIT, PENDING_DEPOSIT};
use crate::merkle::{PendingReceipt, held_deposit_key, keccak, pending_deposit_key, pending_receipt_hash, verify};
use crate::protocol::ProtocolVersion;
use crate::types::AccountId;

/// The kind byte of an unfinalised credit leaf's key.
const UNFINALISED_CREDIT: u8 = 35;

/// An unfinalised credit's leaf with its proof, as the vault's exit counts the account's unfinalised
/// principal through it: its value binds the deposit id, beneficiary, payer and amount as a pending
/// deposit's leaf does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnfinalisedCreditProof {
    pub seq: u64,
    pub root: B256,
    /// The receipt's reference, whose UTF-8 bytes key the leaf.
    pub chain_ref: String,
    pub deposit: PendingReceipt,
    pub siblings: Vec<B256>,
}

/// The receipt the last root holds at a vault deposit id, with its proof, as the vault's
/// `refundReceipt` takes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptProof {
    pub seq: u64,
    pub root: B256,
    pub kind: ReceiptKind,
    /// The receipt's reference, whose UTF-8 bytes key the leaf.
    pub chain_ref: String,
    pub deposit: PendingReceipt,
    pub siblings: Vec<B256>,
}

/// Which leaf a receipt the root holds at a vault deposit id has: from `V77` a held deposit's (a
/// receipt-backed one's), a pending deposit's, or an unfinalised credit's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptKind {
    Held,
    Pending,
    Credited,
}

/// The unfinalised credits as the last root holds them, and their references by beneficiary, the
/// house's under `None`.
#[derive(Debug, Default)]
pub(crate) struct CreditLeaves {
    credits: BTreeMap<String, PendingReceipt>,
    by_account: BTreeMap<Option<AccountId>, BTreeSet<String>>,
}

impl UnfinalisedCreditProof {
    /// Whether the credit proves against `root`, which the caller takes from a posted checkpoint.
    pub fn verify(&self, root: B256) -> bool {
        verify(root, unfinalised_credit_key(&self.chain_ref), pending_receipt_hash(&self.deposit), &self.siblings)
    }
}

impl ReceiptProof {
    /// Whether the receipt's leaf proves against `root`.
    pub fn verify(&self, root: B256) -> bool {
        verify(root, self.kind.key(&self.chain_ref), pending_receipt_hash(&self.deposit), &self.siblings)
    }
}

impl ReceiptKind {
    /// The kind byte of the leaf's key, which the vault's `refundReceipt` takes.
    pub fn leaf_kind(self) -> u8 {
        match self {
            Self::Held => HELD_DEPOSIT,
            Self::Pending => PENDING_DEPOSIT,
            Self::Credited => UNFINALISED_CREDIT,
        }
    }

    /// The key of this kind of leaf for the receipt at `chain_ref`.
    fn key(self, chain_ref: &str) -> B256 {
        match self {
            Self::Held => held_deposit_key(chain_ref),
            Self::Pending => pending_deposit_key(chain_ref),
            Self::Credited => unfinalised_credit_key(chain_ref),
        }
    }
}

impl CreditLeaves {
    /// Brings the committed credits up to `deposits` at `changed`, the receipt references changed
    /// since the last root, and returns each one's leaf key and value hash, `None` where the receipt
    /// holds no unfinalised credit.
    pub(super) fn update(
        &mut self,
        deposits: &BTreeMap<String, RecordedDeposit>,
        changed: &BTreeSet<String>,
    ) -> Vec<(B256, Option<B256>)> {
        changed
            .iter()
            .map(|reference| {
                let new = deposits.get(reference).and_then(unfinalised_credit);
                let old = match new {
                    Some(credit) => self.credits.insert(reference.clone(), credit),
                    None => self.credits.remove(reference),
                };
                if let Some(old) = old {
                    unindex(&mut self.by_account, old.account, reference);
                }
                if let Some(new) = new {
                    self.by_account.entry(new.account).or_default().insert(reference.clone());
                }
                (unfinalised_credit_key(reference), new.as_ref().map(pending_receipt_hash))
            })
            .collect()
    }

    /// The committed credits for `beneficiary`, `None` for the house, in reference order.
    pub(super) fn for_account(
        &self,
        beneficiary: Option<AccountId>,
    ) -> impl Iterator<Item = (&String, &PendingReceipt)> {
        self.by_account.get(&beneficiary).into_iter().flatten().map(|reference| (reference, &self.credits[reference]))
    }

    /// The committed credit at vault deposit id `deposit_id`, if the root holds one there.
    fn at(&self, deposit_id: u64) -> Option<(&String, &PendingReceipt)> {
        self.credits.iter().find(|(_, credit)| credit.deposit_id == deposit_id)
    }
}

impl Engine {
    /// The proofs of `account`'s unfinalised credits against the last root, in increasing deposit
    /// id, as the vault's `registerExitCredits` takes them; empty before the first root under `V77`.
    pub(super) fn unfinalised_credit_proofs(&self, account: AccountId) -> Vec<UnfinalisedCreditProof> {
        let Some(state) = self.tree.as_ref() else { return Vec::new() };
        let mut proofs: Vec<UnfinalisedCreditProof> = state
            .exit
            .credits
            .for_account(Some(account))
            .map(|(reference, credit)| UnfinalisedCreditProof {
                seq: state.seq,
                root: state.root,
                chain_ref: reference.clone(),
                deposit: *credit,
                siblings: state
                    .tree
                    .proof(unfinalised_credit_key(reference))
                    .expect("every committed credit is in the tree"),
            })
            .collect();
        proofs.sort_by_key(|proof| proof.deposit.deposit_id);
        proofs
    }

    /// The proof of the receipt the last root holds at vault deposit id `deposit_id`, credited and
    /// unfinalised, pending or, from a root computed under `V77`, held, through which the vault's exit
    /// refunds the deposit at that id when its records do not match. `None` before the first root
    /// under `V38`, or when the root holds no such receipt there. The work is a scan of the committed
    /// credits, pending and held deposits, which only receipts awaiting finality or screening, or held,
    /// make.
    pub fn receipt_proof(&self, deposit_id: u64) -> Option<ReceiptProof> {
        let state = self.tree.as_ref()?;
        let exit = &state.exit;
        exit.terms.as_ref()?;
        let (kind, (reference, deposit)) = exit
            .credits
            .at(deposit_id)
            .map(|(reference, credit)| (ReceiptKind::Credited, (reference, *credit)))
            .or_else(|| exit.pending_at(deposit_id).map(|found| (ReceiptKind::Pending, found)))
            .or_else(|| {
                let held = exit.held_at(deposit_id).filter(|_| state.version >= ProtocolVersion::V77);
                held.map(|found| (ReceiptKind::Held, found))
            })?;
        let siblings = state.tree.proof(kind.key(reference)).expect("every committed exit leaf is in the tree");
        Some(ReceiptProof { seq: state.seq, root: state.root, kind, chain_ref: reference.clone(), deposit, siblings })
    }

    /// Records every unfinalised receipt and every held deposit for the state root, so the next root
    /// commits the credit leaf of each credited one and the ABI leaf of each receipt-backed hold, as
    /// `V77`'s activation does.
    pub(crate) fn touch_exit_receipts(&mut self) {
        self.ensure_hot_indexes();
        let unfinalised = &self.hot_indexes.as_ref().expect("indexes initialised").pending_deposits;
        let references: Vec<String> = unfinalised.iter().cloned().collect();
        for reference in references {
            self.deposits.get_mut(&reference);
        }
        let holds: Vec<String> = self.held_deposits.keys().cloned().collect();
        for reference in holds {
            self.held_deposits.get_mut(&reference);
        }
    }
}

/// The unfinalised credit leaf's key, `keccak256(abi.encodePacked(uint8(35), bytes(chainRef)))`.
pub fn unfinalised_credit_key(chain_ref: &str) -> B256 {
    keccak(&[&[UNFINALISED_CREDIT], chain_ref.as_bytes()])
}

/// The leaf `recorded` holds while it is credited and unfinalised: its principal is cash, locked
/// until finality.
fn unfinalised_credit(recorded: &RecordedDeposit) -> Option<PendingReceipt> {
    let credited = recorded.status == DepositStatus::Unfinalised && recorded.uncredited.is_none();
    credited.then_some(PendingReceipt {
        deposit_id: recorded.receipt.deposit_id,
        account: match recorded.receipt.beneficiary {
            DepositBeneficiary::Trader { account } => Some(account),
            DepositBeneficiary::House => None,
        },
        payer: recorded.payer,
        amount: recorded.receipt.amount,
    })
}
