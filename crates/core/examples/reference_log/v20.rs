//! Receipt principal, finality, reversal and a sequenced canonical frontier under V19.

use std::collections::BTreeMap;

use engine_core::Engine;
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::{
    CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositReject, DepositUpdate, LegacyDeposit,
};
use engine_core::eip712::B256;
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::common::{Harness, f};

/// Records receipt transitions; the chain/quorum boundary is exercised by the dev scenario.
pub(crate) fn session(h: &mut Harness) {
    // This existing fixture signer had only a refused unfunded order in the preceding session.
    let account = h.signer(9);
    assert!(h.engine.accounts().get(&account).is_none_or(|a| a.balance.is_zero() && a.positions.is_empty()));
    h.deposit(account, "40");
    let deposit = receipt(0, account, "100");
    let events = h.cmd(Command::Deposit {
        account,
        amount: deposit.amount,
        chain_ref: format!("{}:{}", deposit.tx_hash, deposit.log_index),
    });
    assert_eq!(events, vec![Event::Deposited { account, amount: deposit.amount }]);
    let (mut replay, mut legacy) = (Engine::new(), BTreeMap::new());
    for command in &h.log {
        let events = replay.apply(command).expect("historical sequence");
        if let Some(deposit) = LegacyDeposit::from_command(&command.cmd, &events) {
            legacy.insert(deposit.chain_ref.clone(), deposit);
        }
    }
    assert_eq!(replay.state_root(), h.engine.state_root());
    h.admin(AdminCommand::ActivateCustodyFinality {
        version: ProtocolVersion::V19,
        legacy_deposits: legacy.into_values().collect(),
    });

    assert_eq!(h.balance(account), f("140"));
    assert_eq!(h.engine.accounts()[&account].unfinalised_deposits, f("100"));
    update(h, DepositUpdate::Credit { receipt: deposit.clone() });
    assert_eq!(h.balance(account), f("140"));
    assert_eq!(h.engine.accounts()[&account].unfinalised_deposits, f("100"));
    let conflicting = DepositReceipt { amount: f("101"), ..deposit.clone() };
    assert_eq!(
        h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: conflicting.clone() }))),
        vec![Event::CustodyDepositRejected { receipt: conflicting, reason: DepositReject::ConflictingReceipt }]
    );
    let events = h.cmd(Command::Withdraw { account, amount: f("41"), auth: None });
    assert!(
        events.iter().any(|event| {
            matches!(event, Event::WithdrawalRejected { reason: Reject::InsufficientFunds { available }, .. }
            if *available == f("40"))
        }),
        "{events:?}"
    );
    withdraw(h, account, "40");
    update(h, DepositUpdate::Finalise { receipt: deposit.clone(), observed_at_seq: 0 });
    assert_eq!(h.balance(account), f("100"), "finality moves no cash");
    assert!(update(h, DepositUpdate::Finalise { receipt: deposit, observed_at_seq: 0 }).is_empty());
    withdraw(h, account, "100");

    let orphan = receipt(1, account, "75");
    update(h, DepositUpdate::Credit { receipt: orphan.clone() });
    update(h, DepositUpdate::Reverse { receipt: orphan.clone(), observed_at_seq: 0 });
    assert!(update(h, DepositUpdate::Reverse { receipt: orphan, observed_at_seq: 0 }).is_empty());
    assert_eq!(h.balance(account), f("0"));
    assert!(h.engine.accounts()[&account].unfinalised_deposits.is_zero());
    assert!(
        h.cmd(Command::CustodyFinalised(CustodyCheckpoint {
            block: 21,
            block_hash: B256::repeat_byte(0x39),
            deposit_count: 1,
        }))
        .is_empty()
    );
    assert_eq!(h.engine.custody_deposit_coverage(), Some(1));

    // A reorg reincludes a live receipt's transaction in another block: one step moves it.
    let live = receipt(2, account, "60");
    update(h, DepositUpdate::Credit { receipt: live.clone() });
    let moved = DepositReceipt { block: 24, block_hash: B256::repeat_byte(0x3b), log_index: 3, ..live.clone() };
    let events = update(
        h,
        DepositUpdate::Replace {
            receipt: live,
            observed_at_seq: 0,
            replacement: moved,
            replacement_reversed_at_seq: None,
        },
    );
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(h.balance(account), f("60"));
    assert_eq!(h.engine.accounts()[&account].unfinalised_deposits, f("60"));
    assert_eq!(h.engine.custody_deposit_coverage(), Some(2));
}

fn receipt(id: u64, account: AccountId, amount: &str) -> DepositReceipt {
    DepositReceipt {
        block: 20 + id,
        block_hash: B256::repeat_byte(0x19 + id as u8),
        tx_hash: B256::repeat_byte(0x29 + id as u8),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary: DepositBeneficiary::Trader { account },
        amount: f(amount),
    }
}

fn update(h: &mut Harness, mut update: DepositUpdate) -> Vec<Event> {
    if let Some(record) = h.engine.recorded_deposit(&update.subject().reference()) {
        match &mut update {
            DepositUpdate::Finalise { observed_at_seq, .. }
            | DepositUpdate::Reverse { observed_at_seq, .. }
            | DepositUpdate::Replace { observed_at_seq, .. } => {
                *observed_at_seq = record.updated_at_seq;
            }
            _ => {}
        }
    }
    let events = h.cmd(Command::CustodyDeposit(Box::new(update)));
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    events
}

fn withdraw(h: &mut Harness, account: AccountId, amount: &str) {
    let events = h.cmd(Command::Withdraw { account, amount: f(amount), auth: None });
    assert!(
        events
            .iter()
            .any(|event| { matches!(event, Event::WithdrawalApproved { amount: value, .. } if *value == f(amount)) }),
        "{events:?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_reference_session_uses_actual_v19_without_writing_a_log() {
        let mut h = Harness::signed();
        h.admin(AdminCommand::SetOperator(crate::common::operator()));
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
        session(&mut h);
        assert_eq!(h.engine.protocol_version(), ProtocolVersion::V19);
        assert_eq!(h.engine.custody_deposit_coverage(), Some(1));
    }
}
