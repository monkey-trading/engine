//! Component index tests; sequenced frontier refusals are tested in tests/deposit_frontier.rs.
use super::*;

#[test]
fn canonical_coverage_retains_finalised_receipts_above_the_captured_frontier_and_rebuilds() {
    let mut e = engine();
    let (first, second) = (receipt(0, 100), receipt(1, 50));
    apply(&mut e, DepositUpdate::Credit { receipt: first.clone() });
    apply(&mut e, DepositUpdate::Credit { receipt: second.clone() });
    assert_eq!(e.custody_deposit_coverage(), None, "no canonical frontier proof yet");
    e.custody_checkpoint = Some(CustodyCheckpoint { block: 9, block_hash: B256::repeat_byte(9), deposit_count: 0 });
    assert_eq!(e.custody_deposit_coverage(), Some(2));
    apply(&mut e, DepositUpdate::Finalise { receipt: second.clone(), observed_at_seq: 0 });
    assert_eq!(e.custody_deposit_coverage(), Some(2), "finality above an older frontier must not remove coverage");
    apply(&mut e, DepositUpdate::Reverse { receipt: first.clone(), observed_at_seq: 0 });
    assert_eq!(e.custody_deposit_coverage(), Some(1));
    let reversed_at_seq = e.recorded_deposit(&first.reference()).unwrap().updated_at_seq;
    apply(&mut e, DepositUpdate::Reinstate { receipt: first.clone(), reversed_at_seq });
    assert_eq!(e.custody_deposit_coverage(), Some(2));
    apply(&mut e, DepositUpdate::Finalise { receipt: first.clone(), observed_at_seq: 0 });
    e.hot_indexes.as_mut().unwrap().cover_deposits_through(first.block);
    e.custody_checkpoint =
        Some(CustodyCheckpoint { block: first.block, block_hash: first.block_hash, deposit_count: 1 });
    assert_eq!(e.custody_deposit_coverage(), Some(2), "only the covered prefix is retired");
    let mut rebuilt: Engine = serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
    assert_eq!(rebuilt.custody_deposit_coverage(), None, "unbuilt derived state supplies no proof");
    rebuilt.ensure_hot_indexes();
    assert_eq!(rebuilt.custody_deposit_coverage(), Some(2));
    rebuilt.hot_indexes.as_mut().unwrap().cover_deposits_through(second.block);
    rebuilt.custody_checkpoint =
        Some(CustodyCheckpoint { block: second.block, block_hash: second.block_hash, deposit_count: 2 });
    assert_eq!(rebuilt.custody_deposit_coverage(), Some(2));
}

#[test]
fn historical_reference_tombstones_are_not_canonical_coverage_or_finality() {
    let mut e = engine();
    let d = receipt(0, 100);
    let reference = format!("{}:{}", d.tx_hash, d.log_index);
    let mut events = Vec::new();
    let command = Command::Deposit { account: TRADER, amount: d.amount, chain_ref: reference };
    if let Command::Deposit { account, amount, chain_ref } = &command {
        e.deposit(*account, *amount, chain_ref, &mut events);
    }
    let old = LegacyDeposit::from_command(&command, &events).unwrap();
    e.deposit(TRADER, amount(50), "dev:untouched", &mut Vec::new());
    e.lock_legacy_deposits(&[old]).unwrap();
    e.custody_checkpoint = Some(CustodyCheckpoint { block: 9, block_hash: B256::repeat_byte(9), deposit_count: 0 });
    assert_eq!(e.custody_deposit_coverage(), Some(0));
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (amount(150), amount(100)));
    apply(&mut e, DepositUpdate::Credit { receipt: d.clone() });
    assert_eq!(e.custody_deposit_coverage(), Some(1));
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (amount(150), amount(100)));
}

#[test]
fn canonical_count_overflow_supplies_no_coverage_value() {
    let mut e = engine();
    apply(&mut e, DepositUpdate::Credit { receipt: receipt(0, 100) });
    e.custody_checkpoint =
        Some(CustodyCheckpoint { block: 9, block_hash: B256::repeat_byte(9), deposit_count: u64::MAX });
    assert_eq!(e.custody_deposit_coverage(), None);
}
