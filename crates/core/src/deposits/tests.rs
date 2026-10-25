//! Receipt transitions, financial effects and derived index component tests.

use super::*;
use crate::command::{AdminCommand, Command, Sequenced};

const TRADER: AccountId = AccountId([1; 20]);
mod coverage;

#[test]
fn credit_and_finality_change_cash_once_and_release_only_the_matching_principal() {
    let mut e = engine();
    e.accounts.entry(TRADER).or_default().balance = amount(70);
    let (first, second) = (receipt(0, 100), receipt(1, 200));
    let first_events = apply(&mut e, DepositUpdate::Credit { receipt: first.clone() });
    assert_eq!(
        first_events,
        vec![Event::CustodyDepositChanged {
            receipt: first.clone(),
            status: DepositStatus::Unfinalised,
            principal_delta: first.amount,
        }]
    );
    assert!(apply(&mut e, DepositUpdate::Credit { receipt: first.clone() }).is_empty());
    apply(&mut e, DepositUpdate::Credit { receipt: second.clone() });
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (amount(370), amount(300)));
    apply(&mut e, DepositUpdate::Finalise { receipt: first.clone(), observed_at_seq: 0 });
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (amount(370), amount(200)));
    assert!(apply(&mut e, DepositUpdate::Finalise { receipt: first.clone(), observed_at_seq: 0 }).is_empty());
    let events = reject(&mut e, DepositUpdate::Reverse { receipt: first, observed_at_seq: 0 });
    assert_eq!(events, DepositReject::SettledReceipt);
    assert_eq!(e.unfinalised_deposits(0, 20), vec![second]);
}

#[test]
fn reversal_preserves_other_funds_and_flat_losses_are_written_off_once_against_the_house() {
    for (balance_after_trading, final_balance, bad_debt) in [(150, 50, 0), (100, 0, 0), (30, 0, 70)] {
        let mut e = engine();
        e.house_cash = amount(20);
        let d = receipt(0, 100);
        apply(&mut e, DepositUpdate::Credit { receipt: d.clone() });
        e.accounts.get_mut(&TRADER).unwrap().balance = amount(balance_after_trading);
        let events = apply(&mut e, DepositUpdate::Reverse { receipt: d.clone(), observed_at_seq: 0 });
        assert_eq!(e.accounts[&TRADER].balance, amount(final_balance));
        assert_eq!(e.accounts[&TRADER].unfinalised_deposits, Fixed::ZERO);
        assert_eq!(e.house_cash, amount(20 - bad_debt));
        assert_eq!(events.iter().filter(|event| matches!(event, Event::BadDebt { account, amount: a } if *account == TRADER && *a == amount(bad_debt))).count(), usize::from(bad_debt > 0));
        assert!(apply(&mut e, DepositUpdate::Reverse { receipt: d.clone(), observed_at_seq: 0 }).is_empty());
        assert!(
            apply(&mut e, DepositUpdate::Credit { receipt: d }).is_empty(),
            "late duplicate credit cannot resurrect an orphan"
        );
        assert_eq!(e.accounts[&TRADER].balance, amount(final_balance));
    }
}

#[test]
fn a_canonical_replacement_moves_the_receipt_in_one_step_without_cash_or_close_out() {
    let mut e = engine();
    let old = receipt(0, 100);
    apply(&mut e, DepositUpdate::Credit { receipt: old.clone() });
    e.accounts.get_mut(&TRADER).unwrap().balance = amount(30);
    let moved =
        DepositReceipt { block: 25, block_hash: B256::repeat_byte(2), log_index: 87, deposit_id: 300, ..old.clone() };
    let replace = DepositUpdate::Replace {
        receipt: old.clone(),
        observed_at_seq: 0,
        replacement: moved.clone(),
        replacement_reversed_at_seq: None,
    };
    let house = e.house_cash;
    let events = apply(&mut e, replace.clone());
    assert_eq!(
        events,
        vec![
            Event::CustodyDepositChanged {
                receipt: old.clone(),
                status: DepositStatus::Reversed,
                principal_delta: -old.amount
            },
            Event::CustodyDepositChanged {
                receipt: moved.clone(),
                status: DepositStatus::Unfinalised,
                principal_delta: moved.amount,
            },
        ],
        "no close-out or write-off runs on a net-zero move"
    );
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (amount(30), amount(100)));
    assert_eq!(e.house_cash, house);
    assert!(apply(&mut e, replace).is_empty(), "redelivery after the move is a no-op");
    assert!(apply(&mut e, DepositUpdate::Credit { receipt: old.clone() }).is_empty());
    assert!(apply(&mut e, DepositUpdate::Credit { receipt: moved.clone() }).is_empty());
    assert_eq!(e.unfinalised_deposits(0, 30), vec![moved.clone()]);
    // A reorg back to the original block moves it again, reinstating the exact reversed receipt.
    let reversed_at_seq = e.recorded_deposit(&old.reference()).unwrap().updated_at_seq;
    let observed_at_seq = e.recorded_deposit(&moved.reference()).unwrap().updated_at_seq;
    let stale = DepositUpdate::Replace {
        receipt: moved.clone(),
        observed_at_seq,
        replacement: old.clone(),
        replacement_reversed_at_seq: Some(reversed_at_seq - 1),
    };
    assert_eq!(reject(&mut e, stale), DepositReject::SettledReceipt, "the named reversal must be current");
    apply(
        &mut e,
        DepositUpdate::Replace {
            receipt: moved.clone(),
            observed_at_seq: 0,
            replacement: old.clone(),
            replacement_reversed_at_seq: Some(reversed_at_seq),
        },
    );
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (amount(30), amount(100)));
    assert_eq!(e.unfinalised_deposits(0, 30), vec![old.clone()]);
    apply(&mut e, DepositUpdate::Finalise { receipt: old, observed_at_seq: 0 });
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (amount(30), Fixed::ZERO));
}

#[test]
fn a_replacement_must_be_the_same_live_deposit_and_refusals_change_nothing() {
    let mut e = engine();
    let old = receipt(0, 100);
    apply(&mut e, DepositUpdate::Credit { receipt: old.clone() });
    let observed_at_seq = e.recorded_deposit(&old.reference()).unwrap().updated_at_seq;
    let moved = DepositReceipt { block: 25, block_hash: B256::repeat_byte(2), log_index: 87, ..old.clone() };
    let replace = |replacement: DepositReceipt, observed_at_seq, replacement_reversed_at_seq| DepositUpdate::Replace {
        receipt: old.clone(),
        observed_at_seq,
        replacement,
        replacement_reversed_at_seq,
    };
    let other_account = DepositBeneficiary::Trader { account: AccountId([2; 20]) };
    e.custody_checkpoint = Some(CustodyCheckpoint { block: 5, block_hash: B256::repeat_byte(5), deposit_count: 0 });
    for (update, reason) in [
        (replace(moved.clone(), observed_at_seq + 1, None), DepositReject::SettledReceipt),
        (replace(old.clone(), observed_at_seq, None), DepositReject::InvalidReceipt),
        (
            replace(DepositReceipt { block_hash: B256::ZERO, ..moved.clone() }, observed_at_seq, None),
            DepositReject::InvalidReceipt,
        ),
        (
            replace(DepositReceipt { amount: amount(101), ..moved.clone() }, observed_at_seq, None),
            DepositReject::ConflictingReceipt,
        ),
        (
            replace(DepositReceipt { beneficiary: other_account, ..moved.clone() }, observed_at_seq, None),
            DepositReject::ConflictingReceipt,
        ),
        (
            replace(DepositReceipt { transaction_deposit_index: 1, ..moved.clone() }, observed_at_seq, None),
            DepositReject::ConflictingReceipt,
        ),
        (
            replace(DepositReceipt { tx_hash: B256::repeat_byte(77), ..moved.clone() }, observed_at_seq, None),
            DepositReject::ConflictingReceipt,
        ),
        (replace(moved.clone(), observed_at_seq, Some(observed_at_seq)), DepositReject::UnknownReceipt),
        (replace(DepositReceipt { block: 5, ..moved.clone() }, observed_at_seq, None), DepositReject::SettledReceipt),
        (
            DepositUpdate::Replace {
                receipt: receipt(9, 100),
                observed_at_seq,
                replacement: moved.clone(),
                replacement_reversed_at_seq: None,
            },
            DepositReject::UnknownReceipt,
        ),
    ] {
        let root = e.state_root();
        assert_eq!(reject(&mut e, update), reason);
        assert_eq!(e.state_root(), root);
        assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (amount(100), amount(100)));
    }
}

#[test]
fn transaction_local_identity_prevents_double_credit_and_distinguishes_multiple_deposits() {
    let mut e = engine();
    let first = receipt(0, 100);
    apply(&mut e, DepositUpdate::Credit { receipt: first.clone() });
    let replacement = DepositReceipt { block_hash: B256::repeat_byte(3), ..first.clone() };
    assert_eq!(reject(&mut e, DepositUpdate::Credit { receipt: replacement }), DepositReject::ConflictingReceipt);
    let second = DepositReceipt { log_index: 2, transaction_deposit_index: 1, deposit_id: 1, ..first };
    apply(&mut e, DepositUpdate::Credit { receipt: second });
    assert_eq!(e.accounts[&TRADER].balance, amount(200));
}

#[test]
fn exact_block_reappearance_requires_the_current_reversal_sequence_and_restores_once() {
    let mut e = engine();
    let d = receipt(0, 100);
    apply(&mut e, DepositUpdate::Credit { receipt: d.clone() });
    apply(&mut e, DepositUpdate::Reverse { receipt: d.clone(), observed_at_seq: 0 });
    let reversed_at_seq = e.recorded_deposit(&d.reference()).unwrap().updated_at_seq;
    let restore = DepositUpdate::Reinstate { receipt: d.clone(), reversed_at_seq };
    apply(&mut e, restore.clone());
    assert!(apply(&mut e, restore.clone()).is_empty());
    assert_eq!(e.accounts[&TRADER].balance, amount(100));
    apply(&mut e, DepositUpdate::Reverse { receipt: d.clone(), observed_at_seq: 0 });
    assert_eq!(reject(&mut e, restore), DepositReject::SettledReceipt);
    assert_eq!(e.accounts[&TRADER].balance, Fixed::ZERO);
}

#[test]
fn invalid_or_conflicting_receipts_and_overflow_leave_cash_and_locks_unchanged() {
    let mut e = engine();
    let d = receipt(0, 100);
    for invalid in [Fixed::ZERO, -Fixed::ONE, Fixed::MAX_INPUT + Fixed::ONE] {
        assert_eq!(
            reject(&mut e, DepositUpdate::Credit { receipt: DepositReceipt { amount: invalid, ..d.clone() } }),
            DepositReject::InvalidAmount
        );
    }
    for invalid in [
        DepositReceipt { block_hash: B256::ZERO, ..d.clone() },
        DepositReceipt { tx_hash: B256::ZERO, ..d.clone() },
        DepositReceipt { block: u64::MAX, ..d.clone() },
    ] {
        assert_eq!(reject(&mut e, DepositUpdate::Credit { receipt: invalid }), DepositReject::InvalidReceipt);
    }
    assert_eq!(
        reject(&mut e, DepositUpdate::Finalise { receipt: d.clone(), observed_at_seq: 0 }),
        DepositReject::UnknownReceipt
    );
    e.accounts.entry(TRADER).or_default().balance = Fixed::from_raw(i128::MAX);
    let before = serde_json::to_vec(&e.accounts).unwrap();
    assert_eq!(reject(&mut e, DepositUpdate::Credit { receipt: d.clone() }), DepositReject::ArithmeticOverflow);
    assert_eq!(serde_json::to_vec(&e.accounts).unwrap(), before);
    assert!(e.deposits.is_empty());
    e.accounts.get_mut(&TRADER).unwrap().balance = Fixed::ZERO;
    apply(&mut e, DepositUpdate::Credit { receipt: d.clone() });
    assert_eq!(
        reject(
            &mut e,
            DepositUpdate::Finalise { receipt: DepositReceipt { amount: amount(99), ..d }, observed_at_seq: 0 }
        ),
        DepositReject::ConflictingReceipt
    );
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (amount(100), amount(100)));
}

#[test]
fn receipt_state_survives_serialisation_and_rebuilds_the_same_merkle_root() {
    let mut e = engine();
    e.protocol_version = crate::protocol::ProtocolVersion::V17;
    let d = receipt(0, 100);
    apply(&mut e, DepositUpdate::Credit { receipt: d.clone() });
    let root = e.state_root();
    let mut recovered: Engine = serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
    assert_eq!(recovered.state_root(), root);
    assert!(apply(&mut recovered, DepositUpdate::Credit { receipt: d.clone() }).is_empty());
    apply(&mut recovered, DepositUpdate::Finalise { receipt: d.clone(), observed_at_seq: 0 });
    assert_eq!(recovered.accounts[&TRADER].balance, amount(100));
    assert_eq!(recovered.accounts[&TRADER].unfinalised_deposits, Fixed::ZERO);
    assert_eq!(recovered.recorded_deposit(&d.reference()).unwrap().status, DepositStatus::Finalised);
    let root = recovered.state_root();
    let mut fresh: Engine = serde_json::from_slice(&serde_json::to_vec(&recovered).unwrap()).unwrap();
    assert_eq!(fresh.state_root(), root);
}

#[test]
fn house_reversal_may_make_house_cash_negative_and_is_not_a_capital_withdrawal() {
    let mut e = engine();
    let d = DepositReceipt { beneficiary: DepositBeneficiary::House, ..receipt(0, 100) };
    apply(&mut e, DepositUpdate::Credit { receipt: d.clone() });
    e.house_cash = amount(20);
    apply(&mut e, DepositUpdate::Reverse { receipt: d.clone(), observed_at_seq: 0 });
    assert_eq!((e.house_cash, e.unfinalised_house_deposits), (amount(-80), Fixed::ZERO));
    assert!(apply(&mut e, DepositUpdate::Reverse { receipt: d, observed_at_seq: 0 }).is_empty());
    assert_eq!(e.house_cash, amount(-80));
}

#[test]
fn stale_transitions_cannot_reapply_after_a_canonical_receipt_returns() {
    let mut e = engine();
    let first = receipt(0, 100);
    apply(&mut e, DepositUpdate::Credit { receipt: first.clone() });
    let observed_at_seq = e.recorded_deposit(&first.reference()).unwrap().updated_at_seq;
    let other = DepositReceipt { block_hash: B256::repeat_byte(99), ..first.clone() };
    apply(&mut e, DepositUpdate::Reverse { receipt: first.clone(), observed_at_seq });
    let reversed_at_seq = e.recorded_deposit(&first.reference()).unwrap().updated_at_seq;
    apply(&mut e, DepositUpdate::Credit { receipt: other.clone() });
    apply(&mut e, DepositUpdate::Reverse { receipt: other, observed_at_seq: 0 });
    apply(&mut e, DepositUpdate::Reinstate { receipt: first.clone(), reversed_at_seq });
    for stale in [
        DepositUpdate::Finalise { receipt: first.clone(), observed_at_seq },
        DepositUpdate::Reverse { receipt: first.clone(), observed_at_seq },
    ] {
        assert_eq!(reject(&mut e, stale), DepositReject::SettledReceipt);
        assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (amount(100), amount(100)));
    }
    assert_eq!(e.unfinalised_deposits(0, 100), vec![first]);
    assert!(e.unfinalised_deposits(100, 0).is_empty());
}

#[test]
fn receipt_credit_requires_signed_operator_commands_even_after_reversal() {
    let mut e = engine();
    e.protocol_version = crate::protocol::ProtocolVersion::V17;
    let d = receipt(0, 100);
    apply(&mut e, DepositUpdate::Credit { receipt: d.clone() });
    assert!(e.credited_refs.is_empty(), "new receipts do not use legacy credit references");
    for reverse in [false, true] {
        if reverse {
            apply(&mut e, DepositUpdate::Reverse { receipt: d.clone(), observed_at_seq: 0 });
        }
        let seq = e.last_seq().unwrap() + 1;
        let events = e
            .apply(&Sequenced {
                seq,
                ts: 0,
                cmd: Command::Admin(AdminCommand::SetReduceOnly { market: None, reduce_only: true }),
            })
            .unwrap();
        assert_eq!(
            events,
            vec![Event::AdminRejected {
                reason: "from protocol V8 the operator signs this command once funds are credited".into()
            }]
        );
        assert!(!e.global_reduce_only);
    }
}

#[test]
fn changing_pending_principal_changes_the_account_leaf_commitment() {
    let mut leaf = crate::merkle::AccountLeaf {
        balance: amount(100),
        unfinalised_deposits: amount(25),
        withdrawal_nonce: 0,
        toxicity_bps: 0,
        open_positions: 0,
        toxicity_fraction: Fixed::ZERO,
        toxicity_history: Fixed::ZERO,
    };
    let hash = leaf.value_hash();
    leaf.unfinalised_deposits = amount(26);
    assert_ne!(leaf.value_hash(), hash);
}

#[test]
fn pending_windows_follow_finality_reversal_and_recovery() {
    let mut e = engine();
    let (first, second, third) = (receipt(0, 100), receipt(1, 100), receipt(2, 100));
    for receipt in [&first, &second, &third] {
        apply(&mut e, DepositUpdate::Credit { receipt: receipt.clone() });
    }
    assert_eq!(e.unfinalised_deposits(11, 11), vec![second.clone()]);
    apply(&mut e, DepositUpdate::Finalise { receipt: first, observed_at_seq: 0 });
    apply(&mut e, DepositUpdate::Reverse { receipt: second, observed_at_seq: 0 });
    assert_eq!(e.unfinalised_deposits(0, 100), vec![third.clone()]);
    let mut recovered: Engine = serde_json::from_slice(&serde_json::to_vec(&e).unwrap()).unwrap();
    assert!(recovered.hot_indexes.is_none());
    assert_eq!(recovered.unfinalised_deposits(0, 100), vec![third.clone()]);
    apply(&mut recovered, DepositUpdate::Finalise { receipt: third, observed_at_seq: 0 });
    assert!(recovered.unfinalised_deposits(0, 100).is_empty());
    assert_eq!(
        (recovered.accounts[&TRADER].balance, recovered.accounts[&TRADER].unfinalised_deposits),
        (amount(200), Fixed::ZERO)
    );
}

#[test]
fn legacy_credit_without_migration_evidence_is_never_grandfathered_as_final() {
    let mut e = engine();
    let d = receipt(0, 100);
    let reference = format!("{}:{}", d.tx_hash, d.log_index);
    let mut events = Vec::new();
    e.deposit(TRADER, d.amount, &reference, &mut events);
    assert_eq!(reject(&mut e, DepositUpdate::Credit { receipt: d.clone() }), DepositReject::ConflictingReceipt);
    assert!(e.recorded_deposit(&d.reference()).is_none());
    assert_eq!((e.accounts[&TRADER].balance, e.accounts[&TRADER].unfinalised_deposits), (amount(100), Fixed::ZERO));
}

#[test]
fn old_protocol_rejects_new_custody_commands_without_recording_principal() {
    let mut e = engine();
    let d = receipt(0, 100);
    let events = e
        .apply(&Sequenced {
            seq: 1,
            ts: 0,
            cmd: Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: d.clone() })),
        })
        .unwrap();
    assert_eq!(events, vec![Event::CustodyDepositRejected { receipt: d, reason: DepositReject::ProtocolNotActive }]);
    assert!(e.accounts.is_empty());
    assert!(e.deposits.is_empty());
}

// ── Helpers ──

fn engine() -> Engine {
    let mut e = Engine::new();
    e.apply(&Sequenced { seq: 0, ts: 0, cmd: Command::Tick }).unwrap();
    e
}

fn amount(value: i64) -> Fixed {
    Fixed::from_int(value)
}

fn receipt(id: u64, cash: i64) -> DepositReceipt {
    DepositReceipt {
        block: 10 + id,
        block_hash: B256::repeat_byte(1),
        tx_hash: B256::repeat_byte(10 + id as u8),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary: DepositBeneficiary::Trader { account: TRADER },
        amount: amount(cash),
    }
}

fn apply(e: &mut Engine, mut update: DepositUpdate) -> Vec<Event> {
    let before = cash_total(e);
    observe(e, &mut update);
    e.last_seq = Some(e.last_seq.unwrap() + 1);
    let mut events = Vec::new();
    e.apply_deposit_update(&update, &mut events).unwrap();
    assert_principal_delta(e, before, &events);
    events
}

pub(super) fn cash_total(e: &Engine) -> Fixed {
    e.accounts.values().fold(e.house_cash, |total, account| total + account.balance)
}

pub(super) fn assert_principal_delta(e: &Engine, before: Fixed, events: &[Event]) {
    let recorded: Fixed = events
        .iter()
        .filter_map(|event| match event {
            Event::CustodyDepositChanged { principal_delta, .. } => Some(*principal_delta),
            _ => None,
        })
        .sum();
    assert_eq!(cash_total(e) - before, recorded, "receipt output must equal independent net ledger cash change");
}

fn reject(e: &mut Engine, update: DepositUpdate) -> DepositReject {
    e.apply_deposit_update(&update, &mut Vec::new()).unwrap_err()
}

/// Copies the revision a custody reader observed before sequencing its conditional update.
fn observe(e: &Engine, update: &mut DepositUpdate) {
    let observed = e.recorded_deposit(&update.subject().reference()).map_or(0, |d| d.updated_at_seq);
    match update {
        DepositUpdate::Finalise { observed_at_seq, .. }
        | DepositUpdate::Reverse { observed_at_seq, .. }
        | DepositUpdate::Replace { observed_at_seq, .. }
            if *observed_at_seq == 0 =>
        {
            *observed_at_seq = observed
        }
        _ => {}
    }
}
