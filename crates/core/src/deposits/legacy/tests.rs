use super::*;
use crate::access::CreditScreening;
use crate::deposits::{DepositStatus, DepositUpdate};
use crate::protocol::ProtocolVersion;
use crate::types::AccountId;

const TRADER: AccountId = AccountId([7; 20]);

#[test]
fn legacy_adoption_locks_principal_without_cash_and_only_finality_releases_it() {
    let (mut e, old, receipt) = legacy();
    e.deposit(TRADER, cash(50), "dev:unrelated", &mut Vec::new());
    e.lock_legacy_deposits(std::slice::from_ref(&old)).unwrap();
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (cash(150), cash(100)));
    transition(&mut e, DepositUpdate::Credit { receipt: receipt.clone() });
    assert_eq!(e.recorded_deposit(&receipt.reference()).unwrap().status, DepositStatus::Unfinalised);
    assert!(e.unresolved_legacy_deposit().is_none());
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (cash(150), cash(100)));
    let seq = e.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    transition(&mut e, DepositUpdate::Finalise { receipt: receipt.clone(), observed_at_seq: seq });
    transition(&mut e, DepositUpdate::Credit { receipt });
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (cash(150), Fixed::ZERO));
}

#[test]
fn independent_final_legacy_funds_release_without_unlocking_an_unresolved_credit() {
    let (mut e, old, first) = legacy();
    let mut second = first.clone();
    second.tx_hash = B256::repeat_byte(3);
    second.amount = cash(50);
    let second_ref = format!("{}:{}", second.tx_hash, second.log_index);
    e.deposit(TRADER, second.amount, &second_ref, &mut Vec::new());
    let known = LegacyDeposit { chain_ref: second_ref, beneficiary: second.beneficiary, amount: second.amount };
    e.lock_legacy_deposits(&[old.clone(), known]).unwrap();
    transition(&mut e, DepositUpdate::Credit { receipt: second.clone() });
    let seq = e.recorded_deposit(&second.reference()).unwrap().updated_at_seq;
    transition(&mut e, DepositUpdate::Finalise { receipt: second, observed_at_seq: seq });
    assert_eq!(e.unresolved_legacy_deposit(), Some(&old));
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (cash(150), cash(100)));
}

#[test]
fn legacy_house_capital_and_mock_credits_are_distinct_and_apply_no_cash_twice() {
    let (mut e, old, mut receipt) = legacy();
    receipt.tx_hash = B256::repeat_byte(4);
    receipt.beneficiary = DepositBeneficiary::House;
    let reference = format!("{}:{}", receipt.tx_hash, receipt.log_index);
    let mut events = Vec::new();
    e.house_capital(receipt.amount, &reference, &mut events);
    let house =
        LegacyDeposit::from_command(&Command::HouseCapital { amount: receipt.amount, chain_ref: reference }, &events)
            .unwrap();
    e.deposit(TRADER, cash(25), "dev:mock", &mut Vec::new());
    e.lock_legacy_deposits(&[old, house]).unwrap();
    assert_eq!((e.accounts[&TRADER].unfinalised_deposits, e.unfinalised_house_deposits), (cash(100), cash(100)));
    let house_cash = e.house_cash;
    transition(&mut e, DepositUpdate::Credit { receipt });
    assert_eq!(e.house_cash, house_cash);
    assert_eq!(e.accounts[&TRADER].balance, cash(125));
}

#[test]
fn changed_or_missing_legacy_receipts_never_credit_again_or_release_the_lock() {
    let (mut e, old, receipt) = legacy();
    e.lock_legacy_deposits(std::slice::from_ref(&old)).unwrap();
    for candidate in [
        DepositReceipt { amount: cash(101), ..receipt.clone() },
        DepositReceipt { beneficiary: DepositBeneficiary::House, ..receipt.clone() },
        DepositReceipt { log_index: receipt.log_index + 1, ..receipt.clone() },
    ] {
        assert_eq!(
            e.apply_deposit_update(
                &DepositUpdate::Credit { receipt: candidate },
                CreditScreening::default(),
                &mut Vec::new()
            ),
            Err(DepositReject::ConflictingReceipt)
        );
    }
    assert_eq!(e.unresolved_legacy_deposit(), Some(&old));
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (cash(100), cash(100)));
    assert!(e.deposits.is_empty());
}

#[test]
fn legacy_migration_validation_is_atomic_for_incomplete_duplicate_invalid_or_overflowing_input() {
    let (mut e, old, _) = legacy();
    let original = serde_json::to_vec(&e).unwrap();
    for records in [
        vec![],
        vec![old.clone(), old.clone()],
        vec![LegacyDeposit { amount: Fixed::ZERO, ..old.clone() }],
        vec![LegacyDeposit { beneficiary: DepositBeneficiary::Trader { account: AccountId([9; 20]) }, ..old.clone() }],
    ] {
        assert!(e.lock_legacy_deposits(&records).is_err());
        assert_eq!(serde_json::to_vec(&e).unwrap(), original);
    }
    e.accounts.get_mut(&TRADER).unwrap().unfinalised_deposits = Fixed::from_raw(i128::MAX - 1);
    let original = serde_json::to_vec(&e).unwrap();
    assert!(e.lock_legacy_deposits(&[old]).unwrap_err().contains("overflow"));
    assert_eq!(serde_json::to_vec(&e).unwrap(), original);
}

#[test]
fn pending_legacy_state_and_adoption_leaf_removal_survive_serialisation_and_merkle_rebuild() {
    let (mut e, old, receipt) = legacy();
    let before = e.state_root();
    e.lock_legacy_deposits(std::slice::from_ref(&old)).unwrap();
    let locked = e.state_root();
    assert_ne!(before, locked);
    let mut restored: Engine = serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
    assert_eq!(restored.state_root(), locked);
    assert_eq!(restored.unresolved_legacy_deposit(), Some(&old));
    transition(&mut e, DepositUpdate::Credit { receipt: receipt.clone() });
    let root = e.state_root();
    assert!(e.unresolved_legacy_deposit().is_none());
    let mut rebuilt: Engine = serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
    assert_eq!(rebuilt.state_root(), root, "adoption removes the legacy leaf from incremental and rebuilt roots");
    assert_eq!(rebuilt.accounts[&TRADER].unfinalised_deposits, cash(100));
    assert_eq!(rebuilt.recorded_deposit(&receipt.reference()).unwrap().status, DepositStatus::Unfinalised);
}

#[test]
fn legacy_receipt_amounts_are_committed_independently_of_the_aggregate_principal_lock() {
    let (mut e, old, mut receipt) = legacy();
    receipt.tx_hash = B256::repeat_byte(3);
    receipt.amount = cash(50);
    let second = LegacyDeposit {
        chain_ref: format!("{}:{}", receipt.tx_hash, receipt.log_index),
        beneficiary: receipt.beneficiary,
        amount: receipt.amount,
    };
    e.deposit(TRADER, second.amount, &second.chain_ref, &mut Vec::new());
    e.lock_legacy_deposits(&[old.clone(), second.clone()]).unwrap();
    let root = e.state_root();
    e.legacy_deposits.get_mut(&old.chain_ref).unwrap().amount = cash(99);
    e.legacy_deposits.get_mut(&second.chain_ref).unwrap().amount = cash(51);
    assert_eq!(e.accounts[&TRADER].unfinalised_deposits, cash(150));
    assert_ne!(e.state_root(), root, "separate legacy credit records must be committed, beyond their sum");
}

#[test]
fn an_adopted_legacy_receipt_reverses_once_and_a_proven_replacement_can_credit() {
    let (mut e, old, receipt) = legacy();
    e.lock_legacy_deposits(&[old]).unwrap();
    transition(&mut e, DepositUpdate::Credit { receipt: receipt.clone() });
    e.accounts.get_mut(&TRADER).unwrap().balance = cash(25);
    let seq = e.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    let reverse = DepositUpdate::Reverse { receipt: receipt.clone(), observed_at_seq: seq };
    let events = transition(&mut e, reverse.clone());
    assert!(events.iter().any(|event| matches!(event, Event::BadDebt { amount, .. } if *amount == cash(75))));
    assert!(transition(&mut e, reverse).is_empty());
    assert_eq!(e.house_cash, cash(925));
    let replacement = DepositReceipt { block: 9, block_hash: B256::repeat_byte(9), log_index: 8, ..receipt };
    transition(&mut e, DepositUpdate::Credit { receipt: replacement });
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (cash(100), cash(100)));
}

#[test]
fn only_successful_evm_credits_enter_the_legacy_activation_ledger() {
    let (_, old, _) = legacy();
    let command = Command::Deposit { account: TRADER, amount: old.amount, chain_ref: old.chain_ref.clone() };
    assert_eq!(
        LegacyDeposit::from_command(&command, &[Event::Deposited { account: TRADER, amount: old.amount }]),
        Some(old)
    );
    assert_eq!(LegacyDeposit::from_command(&command, &[]), None);
    assert_eq!(
        LegacyDeposit::from_command(
            &Command::Deposit { account: TRADER, amount: cash(100), chain_ref: "dev:mock".into() },
            &[Event::Deposited { account: TRADER, amount: cash(100) }]
        ),
        None
    );
    for invalid in ["dev:123:abc", "0x01:0", "0x01:00", "genesis-house-capital"] {
        assert!(legacy_chain_reference(invalid).is_none());
    }
}

#[test]
fn inherited_release_proof_requires_the_current_exact_finalised_receipt_and_matching_economics() {
    let (mut e, old, receipt) = legacy();
    e.lock_legacy_deposits(std::slice::from_ref(&old)).unwrap();
    assert_eq!(e.unresolved_legacy_credit(&old.chain_ref), Some(&old));
    assert!(!e.legacy_credit_finalised(&old));
    transition(&mut e, DepositUpdate::Credit { receipt: receipt.clone() });
    assert_eq!(e.unresolved_legacy_credit(&old.chain_ref), None);
    assert!(!e.legacy_credit_finalised(&old), "canonical adoption alone is not finality");
    let seq = e.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    transition(&mut e, DepositUpdate::Finalise { receipt: receipt.clone(), observed_at_seq: seq });
    assert!(e.legacy_credit_finalised(&old));
    for wrong in [
        LegacyDeposit { amount: cash(99), ..old.clone() },
        LegacyDeposit { beneficiary: DepositBeneficiary::House, ..old.clone() },
        LegacyDeposit { chain_ref: format!("{}:2", receipt.tx_hash), ..old.clone() },
        LegacyDeposit { chain_ref: "host:final".into(), ..old.clone() },
    ] {
        assert!(!e.legacy_credit_finalised(&wrong));
    }
    let mut rebuilt: Engine = serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
    rebuilt.state_root();
    assert!(rebuilt.legacy_credit_finalised(&old), "proof comes from committed current receipt state");
}

#[test]
fn an_old_terminal_receipt_cannot_prove_a_changed_current_transaction_receipt() {
    let (mut e, old, receipt) = legacy();
    e.lock_legacy_deposits(std::slice::from_ref(&old)).unwrap();
    transition(&mut e, DepositUpdate::Credit { receipt: receipt.clone() });
    let seq = e.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    transition(&mut e, DepositUpdate::Reverse { receipt: receipt.clone(), observed_at_seq: seq });
    let moved = DepositReceipt { block: 9, block_hash: B256::repeat_byte(9), log_index: 8, ..receipt };
    transition(&mut e, DepositUpdate::Credit { receipt: moved.clone() });
    let seq = e.recorded_deposit(&moved.reference()).unwrap().updated_at_seq;
    transition(&mut e, DepositUpdate::Finalise { receipt: moved, observed_at_seq: seq });
    assert!(
        !e.legacy_credit_finalised(&old),
        "transaction equality is insufficient when the old global log index changed"
    );
    assert_eq!(e.accounts[&TRADER].balance, cash(100));
}

fn cash(n: i64) -> Fixed {
    Fixed::from_int(n)
}

fn legacy() -> (Engine, LegacyDeposit, DepositReceipt) {
    let mut e = Engine {
        protocol_version: ProtocolVersion::V17,
        house_cash: cash(1000),
        last_seq: Some(7),
        ..Engine::default()
    };
    let receipt = DepositReceipt {
        block: 5,
        block_hash: B256::repeat_byte(5),
        tx_hash: B256::repeat_byte(2),
        log_index: 1,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: DepositBeneficiary::Trader { account: TRADER },
        amount: cash(100),
    };
    let old = LegacyDeposit {
        chain_ref: format!("{}:{}", receipt.tx_hash, receipt.log_index),
        beneficiary: receipt.beneficiary,
        amount: receipt.amount,
    };
    e.deposit(TRADER, old.amount, &old.chain_ref, &mut Vec::new());
    (e, old, receipt)
}

fn transition(e: &mut Engine, update: DepositUpdate) -> Vec<Event> {
    let before = crate::deposits::tests::cash_total(e);
    e.last_seq = Some(e.last_seq.unwrap() + 1);
    let mut events = Vec::new();
    e.apply_deposit_update(&update, CreditScreening::default(), &mut events).unwrap();
    crate::deposits::tests::assert_principal_delta(e, before, &events);
    events
}
