//! V22 payout capacity counts only final principal: a receipt credited before Ethereum finality is
//! trading collateral but pays no queued withdrawal until it finalises, and a reversal pays none.

mod common;
#[path = "withdrawals/setup.rs"]
mod setup;

use alloy_primitives::B256;
use common::{Harness, acct, f};
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use setup::{approvals, deposit, predecessor, v22, win, withdraw};

#[test]
fn a_queued_head_waits_for_receipt_finality_and_a_reversal_pays_nothing() {
    let mut h = v22();
    let (trader, depositor) = (acct(1), acct(2));
    deposit(&mut h, trader, "100");
    win(&mut h, trader);
    assert_eq!(h.engine.withdrawal_cash(), Some(f("110")));
    let events = withdraw(&mut h, trader, "200");
    assert_eq!(approvals(&events), vec![(trader, f("110"), 1)]);
    let pending = receipt(1, depositor, "50");
    let events = update(&mut h, DepositUpdate::Credit { receipt: pending.clone() });
    assert!(approvals(&events).is_empty(), "unfinalised principal pays no queued withdrawal");
    assert_eq!(h.engine.withdrawal_cash(), Some(f("0")));
    let orphan = receipt(2, depositor, "60");
    update(&mut h, DepositUpdate::Credit { receipt: orphan.clone() });
    let events = update(&mut h, DepositUpdate::Reverse { receipt: orphan, observed_at_seq: 0 });
    assert!(approvals(&events).is_empty(), "a reversed receipt was never payout capacity");
    assert_eq!(h.engine.withdrawal_cash(), Some(f("0")));
    let events = update(&mut h, DepositUpdate::Finalise { receipt: pending, observed_at_seq: 0 });
    assert_eq!(approvals(&events), vec![(trader, f("50"), 2)], "finality pays the waiting head");
    assert_eq!(h.engine.withdrawal_queue().next().map(|request| request.remaining), Some(f("40")));
}

#[test]
fn activation_leaves_principal_awaiting_finality_out_of_payout_capacity() {
    let mut h = predecessor();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V20 });
    let (trader, depositor) = (acct(1), acct(2));
    deposit(&mut h, trader, "100");
    let pending = receipt(1, depositor, "200");
    update(&mut h, DepositUpdate::Credit { receipt: pending.clone() });
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V22 });
    assert_eq!(h.engine.withdrawal_cash(), Some(f("110")), "house capital and final deposits only");
    update(&mut h, DepositUpdate::Finalise { receipt: pending, observed_at_seq: 0 });
    assert_eq!(h.engine.withdrawal_cash(), Some(f("310")));
}

fn receipt(id: u64, account: AccountId, amount: &str) -> DepositReceipt {
    DepositReceipt {
        block: id + 1,
        block_hash: B256::repeat_byte(1),
        tx_hash: B256::repeat_byte(id as u8 + 1),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary: DepositBeneficiary::Trader { account },
        amount: f(amount),
    }
}

/// Sequences a custody update at the revision a custody reader would have observed.
fn update(h: &mut Harness, mut update: DepositUpdate) -> Vec<Event> {
    let observed = h.engine.recorded_deposit(&update.subject().reference()).map_or(0, |d| d.updated_at_seq);
    if let DepositUpdate::Finalise { observed_at_seq, .. } | DepositUpdate::Reverse { observed_at_seq, .. } =
        &mut update
    {
        *observed_at_seq = observed;
    }
    let events = h.cmd(Command::CustodyDeposit(Box::new(update)));
    assert!(!events.iter().any(|event| matches!(event, Event::CustodyDepositRejected { .. })), "{events:?}");
    events
}
