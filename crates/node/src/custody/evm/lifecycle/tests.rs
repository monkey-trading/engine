//! Reconciliation distinguishes canonical disappearance, inconsistent logs, and reincluded deposits.

use super::*;
use alloy::primitives::U256;
use engine_core::Fixed;
use engine_core::types::AccountId;

#[test]
fn orphans_pair_with_their_reincluded_deposit_and_otherwise_reverse_only_at_the_head() {
    let first = receipt(&deposit(), 0).unwrap();
    let hashes = BTreeMap::from([(first.block, B256::repeat_byte(99))]);
    let moved = |log_index| DepositReceipt {
        block: first.block + 2000,
        block_hash: B256::repeat_byte(22),
        log_index,
        deposit_id: 42,
        ..first.clone()
    };
    let other = AccountId([9; 20]);
    for at_head in [false, true] {
        let unpaired = if at_head { Orphaned::Reversed } else { Orphaned::Deferred };
        assert_eq!(plan(std::slice::from_ref(&first), &[], &hashes, at_head).unwrap(), vec![(first.clone(), unpaired)]);
        for current in [moved(first.log_index), moved(first.log_index + 7)] {
            assert_eq!(
                plan(std::slice::from_ref(&first), std::slice::from_ref(&current), &hashes, at_head).unwrap(),
                vec![(first.clone(), Orphaned::Replaced(current.clone()))]
            );
        }
        for changed in [
            DepositReceipt { amount: Fixed::from_int(11), ..moved(first.log_index) },
            DepositReceipt { beneficiary: DepositBeneficiary::Trader { account: other }, ..moved(first.log_index) },
        ] {
            assert_eq!(
                plan(std::slice::from_ref(&first), &[changed], &hashes, at_head).unwrap(),
                vec![(first.clone(), Orphaned::Reversed)],
                "a transaction canonical with other economics cannot be replaced later"
            );
        }
        let unrelated = DepositReceipt { tx_hash: B256::repeat_byte(7), ..moved(first.log_index) };
        assert_eq!(
            plan(std::slice::from_ref(&first), &[unrelated], &hashes, at_head).unwrap()[0].1,
            if at_head { Orphaned::Reversed } else { Orphaned::Deferred }
        );
    }
}

#[test]
fn same_hash_absence_or_changed_economics_refuses_the_entire_reconciliation() {
    let first = receipt(&deposit(), 0).unwrap();
    let second = DepositReceipt { tx_hash: B256::repeat_byte(3), ..first.clone() };
    let hashes = BTreeMap::from([(first.block, first.block_hash)]);
    assert!(matches!(
        plan(&[first.clone(), second], &[], &hashes, true),
        Err(PollError::Stalled(crate::custody::stall::Stall::Inconsistent(_)))
    ));
    let changed = DepositReceipt { amount: Fixed::from_int(11), ..first.clone() };
    assert!(matches!(
        plan(std::slice::from_ref(&first), &[changed], &hashes, true),
        Err(PollError::Stalled(crate::custody::stall::Stall::Inconsistent(_)))
    ));
    assert!(plan(std::slice::from_ref(&first), std::slice::from_ref(&first), &hashes, true).unwrap().is_empty());
}

#[test]
fn missing_canonical_proof_and_duplicate_receipt_identities_cannot_reverse() {
    let first = receipt(&deposit(), 0).unwrap();
    assert!(matches!(
        plan(std::slice::from_ref(&first), &[], &BTreeMap::new(), true),
        Err(PollError::Stalled(crate::custody::stall::Stall::Unsettled(_)))
    ));
    let hashes = BTreeMap::from([(first.block, B256::repeat_byte(99))]);
    assert!(matches!(
        plan(&[], &[first.clone(), first.clone()], &hashes, true),
        Err(PollError::Stalled(crate::custody::stall::Stall::Inconsistent(_)))
    ));
    let same_deposit = DepositReceipt { block: first.block + 1, log_index: first.log_index + 1, ..first.clone() };
    assert!(matches!(
        plan(&[], &[first, same_deposit], &hashes, true),
        Err(PollError::Stalled(crate::custody::stall::Stall::Inconsistent(_)))
    ));
}

#[test]
fn unrepresentable_and_out_of_range_receipt_amounts_cannot_advance_the_frontier() {
    for units in [U256::ZERO, U256::from(1_000_000_000_000_001u64), U256::MAX] {
        assert!(matches!(
            receipt(&VaultDeposit { units, ..deposit() }, 0),
            Err(PollError::Stalled(crate::custody::stall::Stall::Inconsistent(_)))
        ));
    }
    let largest = receipt(&VaultDeposit { units: U256::from(1_000_000_000_000_000u64), ..deposit() }, 0).unwrap();
    assert_eq!(largest.amount, Fixed::MAX_INPUT);
    assert_eq!(receipt(&deposit(), 7).unwrap().transaction_deposit_index, 7);
}

fn deposit() -> VaultDeposit {
    VaultDeposit {
        block: 10,
        block_hash: B256::repeat_byte(1),
        tx_hash: B256::repeat_byte(2),
        log_index: 3,
        id: 0,
        beneficiary: Beneficiary::Trader(AccountId([1; 20])),
        payer: None,
        units: U256::from(10_000_000),
    }
}

fn plan(
    pending: &[DepositReceipt],
    canonical: &[DepositReceipt],
    hashes: &BTreeMap<u64, B256>,
    at_head: bool,
) -> Result<Vec<(DepositReceipt, Orphaned)>, PollError> {
    let recorded: Vec<_> = pending
        .iter()
        .map(|receipt| RecordedDeposit {
            receipt: receipt.clone(),
            status: DepositStatus::Unfinalised,
            updated_at_seq: 0,
            uncredited: None,
        })
        .collect();
    reconcile(&recorded, canonical, hashes, at_head)
        .map(|orphans| orphans.into_iter().map(|(recorded, orphaned)| (recorded.receipt.clone(), orphaned)).collect())
}
