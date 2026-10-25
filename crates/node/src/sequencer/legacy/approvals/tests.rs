use super::*;
use alloy::primitives::B256;

#[test]
fn canonical_proof_retains_only_exact_pending_receipts_with_matching_economics() {
    let receipt = receipt();
    let pending = RecordedDeposit { receipt: receipt.clone(), status: DepositStatus::Unfinalised, updated_at_seq: 7 };
    let mut proof = FinalityProof::default();
    proof.include_pending_receipt(&receipt, None);
    assert!(proof.receipts.is_empty(), "uncredited chain history consumes no retained proof storage");
    for status in [DepositStatus::Finalised, DepositStatus::Reversed] {
        proof.include_pending_receipt(&receipt, Some(&RecordedDeposit { status, ..pending.clone() }));
        assert!(proof.receipts.is_empty(), "terminal receipts are not currently locked obligations");
    }
    for changed in [
        DepositReceipt { amount: Fixed::from_int(101), ..receipt.clone() },
        DepositReceipt { beneficiary: DepositBeneficiary::House, ..receipt.clone() },
        DepositReceipt { block_hash: B256::repeat_byte(9), ..receipt.clone() },
        DepositReceipt { log_index: 2, ..receipt.clone() },
    ] {
        proof.include_pending_receipt(&changed, Some(&pending));
        assert!(
            proof.receipts.is_empty(),
            "a shared reference or transaction cannot prove different economics/identity"
        );
    }
    proof.include_pending_receipt(&receipt, Some(&pending));
    proof.include_pending_receipt(&receipt, Some(&pending));
    assert_eq!(proof.receipts, BTreeMap::from([(receipt.reference(), receipt.clone())]));
    assert_eq!(pending.status, DepositStatus::Unfinalised, "a delivery proof cannot mutate core lifecycle state");
}

#[test]
fn independent_receipt_proof_storage_does_not_grow_with_uncredited_chain_history() {
    let receipt = receipt();
    let pending = RecordedDeposit { receipt: receipt.clone(), status: DepositStatus::Unfinalised, updated_at_seq: 7 };
    let mut proof = FinalityProof::default();
    proof.include_pending_receipt(&receipt, Some(&pending));
    for block in 10..1010 {
        proof.include_pending_receipt(&DepositReceipt { block, ..receipt.clone() }, None);
    }
    assert_eq!(proof.receipts, BTreeMap::from([(receipt.reference(), receipt)]));
    assert!(proof.legacy.is_empty(), "receipt retention does not invent legacy finality");
}

/// Component input for the pure retention predicate; this does not activate a protocol version.
fn receipt() -> DepositReceipt {
    DepositReceipt {
        block: 5,
        block_hash: B256::repeat_byte(5),
        tx_hash: B256::repeat_byte(2),
        log_index: 1,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: DepositBeneficiary::Trader { account: AccountId([7; 20]) },
        amount: Fixed::from_int(100),
    }
}
