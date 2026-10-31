use super::*;
use crate::account::Position;
use crate::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, RecordedDeposit};

use alloy_primitives::B256;

#[test]
fn position_membership_tracks_open_flat_and_recovered_positions() {
    let (a, b) = (AccountId([1; 20]), AccountId([2; 20]));
    let mut engine = Engine::new();
    engine.ensure_hot_indexes();
    let position = Position { qty: Fixed::from_int(1), entry: Fixed::from_int(100) };
    engine.accounts.entry(a).or_default().positions.insert(1, position);
    engine.record_position(a, 1);
    engine.accounts.entry(b).or_default().positions.insert(2, position);
    engine.record_position(b, 2);
    assert_eq!(engine.hot_indexes.as_ref().unwrap().holders[&1], BTreeSet::from([a]));
    assert_eq!(engine.hot_indexes.as_ref().unwrap().holders[&2], BTreeSet::from([b]));
    engine.accounts.get_mut(&a).unwrap().positions.remove(&1);
    engine.record_position(a, 1);
    assert!(engine.hot_indexes.as_ref().unwrap().holders[&1].is_empty());

    let mut recovered: Engine = serde_json::from_slice(&serde_json::to_vec(&engine).unwrap()).unwrap();
    assert!(recovered.hot_indexes.is_none());
    recovered.ensure_hot_indexes();
    assert!(!recovered.hot_indexes.as_ref().unwrap().holders.contains_key(&1));
    assert_eq!(recovered.hot_indexes.as_ref().unwrap().holders[&2], BTreeSet::from([b]));
}

#[test]
fn rebuild_initialises_coverage_from_the_finalised_base() {
    // A restore whose first receipt sits just past a large finalised frontier rebuilds in
    // constant index work: every id below the base settled, so the span below it opens no
    // missing entries.
    let mut engine = Engine::new();
    engine.custody_checkpoint =
        Some(CustodyCheckpoint { block: 900, block_hash: B256::repeat_byte(9), deposit_count: 1_000_000 });
    let deposit = recorded(1_000_000, 1_000_000, DepositStatus::Finalised);
    engine.deposits.insert(deposit.receipt.reference(), deposit);
    engine.ensure_hot_indexes();
    let indexes = engine.hot_indexes.as_ref().unwrap();
    assert!(indexes.missing.is_empty(), "the settled span below the base stays covered");
    assert_eq!(indexes.hwm, 1_000_001);
    assert_eq!(indexes.run_insertions, 0, "no id below the base was walked");
}

#[test]
fn rebuild_inserts_a_large_skipped_interval_as_one_run() {
    // With no finalised frontier, a receipt far above the first opens the whole skipped
    // interval as a single merged run, not one entry per settled id.
    let mut engine = Engine::new();
    for deposit in [recorded(0, 1000, DepositStatus::Finalised), recorded(100_000, 101_000, DepositStatus::Finalised)] {
        engine.deposits.insert(deposit.receipt.reference(), deposit);
    }
    engine.ensure_hot_indexes();
    let indexes = engine.hot_indexes.as_ref().unwrap();
    assert_eq!(indexes.missing, BTreeMap::from([(1, 99_999)]), "one run for the whole interval");
    assert_eq!(indexes.run_insertions, 1);
}

#[test]
fn a_skipped_span_merges_a_reversal_run_it_passes_over() {
    // A reversed id above every live id leaves a run; a later receipt's skipped span absorbs
    // it instead of overlapping it, so the runs stay disjoint and no id is named twice.
    let mut engine = Engine::new();
    for deposit in [recorded(3, 1003, DepositStatus::Finalised), recorded(7, 1007, DepositStatus::Reversed)] {
        engine.deposits.insert(deposit.receipt.reference(), deposit);
    }
    engine.ensure_hot_indexes();
    let mut indexes = engine.hot_indexes.take().unwrap();
    assert_eq!(indexes.missing, BTreeMap::from([(0, 2), (7, 7)]));
    indexes.add_above_frontier("0xfeed:9".into(), 20, Fixed::from_int(1));
    assert_eq!(indexes.missing, BTreeMap::from([(0, 2), (4, 19)]));
}

/// A recorded house receipt at vault deposit id `id`, observed at `block`.
fn recorded(id: u64, block: u64, status: DepositStatus) -> RecordedDeposit {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&id.to_be_bytes());
    RecordedDeposit {
        receipt: DepositReceipt {
            block,
            block_hash: B256::from(hash),
            tx_hash: B256::repeat_byte(0x40),
            log_index: 0,
            transaction_deposit_index: 0,
            deposit_id: id,
            beneficiary: DepositBeneficiary::House,
            amount: Fixed::from_int(1),
        },
        status,
        updated_at_seq: 1,
        uncredited: None,
        payer: None,
    }
}
