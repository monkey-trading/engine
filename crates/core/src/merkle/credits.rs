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

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::deposits::{DepositBeneficiary, DepositStatus, RecordedDeposit};
use crate::engine::Engine;
use crate::merkle::leaf::{HELD_DEPOSIT, PENDING_DEPOSIT};
use crate::merkle::{PendingReceipt, held_deposit_key, keccak, pending_deposit_key, pending_receipt_hash, verify};

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

impl Engine {
    /// The proof of the receipt the last root holds at vault deposit id `deposit_id`, credited and
    /// unfinalised, pending or, from a root computed under `V77`, held, through which the vault's exit
    /// refunds the deposit at that id when its records do not match. `None` before the first root
    /// under `V38`, or when the root holds no such receipt there. The work is a scan of the committed
    /// credits, pending and held deposits, which only receipts awaiting finality or screening, or held,
    /// make.
    pub fn receipt_proof(&self, deposit_id: u64) -> Option<ReceiptProof> {
        let (tree, version) = self.committed()?;
        tree.receipt_proof(version, deposit_id)
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
pub(super) fn unfinalised_credit(recorded: &RecordedDeposit) -> Option<PendingReceipt> {
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
