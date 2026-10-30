//! Canonical custody replacement screening and retained terminal holds from V26.

use engine_core::access::{AccessEvidence, SanctionsStatus, Screening};
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712::{self, B256, Signature};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;

use crate::common::{Harness, f};

/// Records a credited payer and a sanctioned canonical replacement, then retains its hold.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V26 });
    let payer = AccountId([0x31; 20]);
    let original = DepositReceipt {
        block: 1_000_000,
        block_hash: B256::repeat_byte(0x31),
        tx_hash: B256::repeat_byte(0x32),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 1_000_000,
        beneficiary: DepositBeneficiary::House,
        amount: f("10"),
    };
    let evidence = screening_evidence(h, &original, payer, SanctionsStatus::Clear);
    let events = h.cmd(Command::AccessCustodyDeposit { receipt: original.clone(), payer: Some(payer), evidence });
    assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { principal_delta, .. } if *principal_delta == original.amount)), "{events:?}");
    let moved = DepositReceipt { block: original.block + 1, block_hash: B256::repeat_byte(0x33), ..original.clone() };
    let update = replacement(h, &original, &moved);
    let evidence = screening_evidence(h, &moved, AccountId([0x34; 20]), SanctionsStatus::Sanctioned);
    let events = h.cmd(Command::AccessCustodyReplacement {
        update: Box::new(update),
        payer: Some(AccountId([0x34; 20])),
        evidence,
    });
    assert!(events.iter().any(|event| matches!(event, Event::DepositHeld { .. })), "{events:?}");
    let update = replacement(h, &moved, &original);
    let evidence = screening_evidence(h, &original, payer, SanctionsStatus::Clear);
    h.cmd(Command::AccessCustodyReplacement { update: Box::new(update), payer: Some(payer), evidence });
    assert!(h.engine.held_deposits().contains_key(&original.reference()));
}

/// Builds the conditional atomic transition between the recorded receipt variants.
fn replacement(h: &Harness, original: &DepositReceipt, moved: &DepositReceipt) -> DepositUpdate {
    DepositUpdate::Replace {
        receipt: original.clone(),
        observed_at_seq: h.engine.recorded_deposit(&original.reference()).unwrap().updated_at_seq,
        replacement: moved.clone(),
        replacement_reversed_at_seq: h
            .engine
            .recorded_deposit(&moved.reference())
            .map(|recorded| recorded.updated_at_seq),
    }
}

/// Signs fresh source evidence for the canonical payer and exact replacement reference.
pub(crate) fn screening_evidence(
    h: &Harness,
    receipt: &DepositReceipt,
    payer: AccountId,
    status: SanctionsStatus,
) -> AccessEvidence {
    let source = SigningKey::from_slice(&[0x77; 32]).unwrap();
    let deposit = engine_core::access::receipt_access_deposit(receipt, Some(payer));
    let mut screening = Screening {
        source: eip712::address(source.verifying_key()),
        request_hash: eip712::deposit_screening_hash(&deposit),
        observed_at: h.ts,
        account: None,
        payer: Some(payer),
        jurisdiction: engine_core::access::Jurisdiction::Allowed,
        account_status: SanctionsStatus::Clear,
        payer_status: status,
        signature: Signature::ZERO,
    };
    screening.signature =
        eip712::sign(&source, &eip712::screening_hash(&h.engine.access_domain().unwrap(), &screening));
    AccessEvidence::Screened(screening)
}
