//! A reorganisation's cash phase, which no valid chain input can make fail, undone in full when it does.

use alloy_primitives::B256;

use super::*;
use crate::command::{Command, Sequenced};
use crate::deposits::{DepositBeneficiary, DepositUpdate};
use crate::types::AccountId;

#[test]
fn a_move_whose_cash_cannot_be_represented_leaves_every_balance_and_receipt_as_it_was() {
    let mut e = Engine::new();
    e.apply(&Sequenced { seq: 0, ts: 0, cmd: Command::Tick }).unwrap();
    // Credited under rules that screen nothing, then moved under `V83`'s.
    e.protocol_version = ProtocolVersion::V23;
    let (first, second) = (AccountId([1; 20]), AccountId([2; 20]));
    let receipts = [receipt(0, first), receipt(1, second)];
    for receipt in &receipts {
        e.last_seq = Some(e.last_seq.unwrap() + 1);
        let mut events = Vec::new();
        e.apply_deposit_update(
            &DepositUpdate::Credit { receipt: receipt.clone() },
            CreditScreening::default(),
            &mut events,
        )
        .unwrap();
    }
    // Both moves lose their credit (no payer holds each deposit), and the second account's cash sits
    // where taking its principal out would overflow.
    e.protocol_version = ProtocolVersion::V83;
    e.accounts.get_mut(&second).unwrap().balance = Fixed::from_raw(i128::MIN + 1);
    let before: Vec<_> = [first, second].iter().map(|account| e.accounts[account].clone()).collect();
    let revisions: Vec<_> = receipts.iter().map(|receipt| e.recorded_deposit(&receipt.reference()).cloned()).collect();
    let moves: Vec<ReceiptMove> = receipts
        .iter()
        .map(|receipt| ReceiptMove {
            receipt: receipt.clone(),
            observed_at_seq: e.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq,
            replacement: DepositReceipt {
                block: receipt.block + 10,
                block_hash: B256::repeat_byte(7),
                ..receipt.clone()
            },
            replacement_reversed_at_seq: None,
            payer: None,
            evidence: None,
        })
        .collect();
    e.last_seq = Some(e.last_seq.unwrap() + 1);
    let mut events = Vec::new();
    e.reorganise_deposits(&moves, &mut events);
    assert_eq!(
        events,
        [Event::CustodyDepositRejected { receipt: receipts[1].clone(), reason: DepositReject::ArithmeticOverflow }]
    );
    let after: Vec<_> = [first, second].iter().map(|account| e.accounts[account].clone()).collect();
    assert_eq!(after, before, "the first move's debit was undone");
    let now: Vec<_> = receipts.iter().map(|receipt| e.recorded_deposit(&receipt.reference()).cloned()).collect();
    assert_eq!(now, revisions);
}

fn receipt(id: u64, account: AccountId) -> DepositReceipt {
    DepositReceipt {
        block: 10 + id,
        block_hash: B256::repeat_byte(1),
        tx_hash: B256::repeat_byte(10 + id as u8),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary: DepositBeneficiary::Trader { account },
        amount: Fixed::from_int(100),
    }
}
