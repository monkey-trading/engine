//! V22 withdrawal approval proofs and separate leaves for public FIFO reservations.

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolValue;
use serde::{Deserialize, Serialize};

use crate::merkle::{json_hash, keccak, verify};
use crate::types::AccountId;
use crate::withdrawal::{QueuedWithdrawal, WithdrawalApproval, usdc_amount};

const WITHDRAWAL_APPROVAL: u8 = 32;
const WITHDRAWAL_QUEUE: u8 = 33;

/// An exact payable approval and its root-to-leaf siblings against the last computed state root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WithdrawalApprovalProof {
    pub seq: u64,
    pub root: B256,
    pub approval: WithdrawalApproval,
    pub siblings: Vec<B256>,
}

impl WithdrawalApprovalProof {
    /// Verifies the approval against the caller's trusted posted root.
    pub fn verify(&self, root: B256) -> bool {
        self.approval.amount.is_positive()
            && usdc_amount(self.approval.amount).is_some()
            && verify(
                root,
                withdrawal_approval_key(self.approval.account, self.approval.nonce),
                withdrawal_approval_hash(&self.approval),
                &self.siblings,
            )
    }
}

/// `keccak256(abi.encodePacked(uint8(32), address(account), uint64(nonce)))`.
pub fn withdrawal_approval_key(account: AccountId, nonce: u64) -> B256 {
    keccak(&[&[WITHDRAWAL_APPROVAL], &account.0, &nonce.to_be_bytes()])
}

/// `keccak256(abi.encode(address account, uint256 amountUsdc, uint64 nonce, uint64 approvedSeq))`.
pub fn withdrawal_approval_hash(approval: &WithdrawalApproval) -> B256 {
    let value =
        (Address::from(approval.account.0), U256::from(approval.amount_usdc()), approval.nonce, approval.approved_seq);
    keccak(&[&value.abi_encode()])
}

/// The reservation's leaf key, keyed by its globally unique request sequence.
pub(crate) fn withdrawal_queue_key(requested_seq: u64) -> B256 {
    keccak(&[&[WITHDRAWAL_QUEUE], &requested_seq.to_be_bytes()])
}

/// The reservation's value, including its account, original amount, remainder and FIFO sequence.
pub(crate) fn withdrawal_queue_hash(queued: &QueuedWithdrawal) -> B256 {
    json_hash(queued)
}
