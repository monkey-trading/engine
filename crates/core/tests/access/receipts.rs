//! Custody receipts keep both rules from `V24`: access screening decides whether a receipt's
//! principal enters cash at all, and receipt finality decides when credited principal is released.

use alloy_primitives::B256;
use engine_core::access::{
    AccessEvidence, AccessReject, SanctionsStatus, SourceFailure, UncreditedReceipt, receipt_access_deposit,
};
use engine_core::command::Command;
use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositStatus, DepositUpdate};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};

use crate::common::{Harness, f};
use crate::support::*;

#[test]
fn a_receipt_is_credited_and_locked_only_on_clear_evidence_and_otherwise_waits_or_is_held() {
    let (mut h, client) = setup();
    attest(&mut h, &client);
    let newcomer = crate::common::Client::new(2).account;
    let sanctioned = AccountId([0x5a; 20]);
    let clear = receipt(1, Some(client.account), "100");
    let undeclared = receipt(2, Some(newcomer), "40");
    let held = receipt(3, Some(client.account), "30");
    let payerless = receipt(4, None, "20");
    let raw = receipt(5, Some(client.account), "10");
    let credited = credit(&mut h, &clear, Some(client.account), SanctionsStatus::Clear);
    assert_eq!(changed(&credited), [(DepositStatus::Unfinalised, f("100"), None)]);
    assert_eq!(h.engine.accounts()[&client.account].balance, f("100"));
    assert_eq!(h.engine.accounts()[&client.account].unfinalised_deposits, f("100"));
    let events = credit(&mut h, &undeclared, Some(newcomer), SanctionsStatus::Clear);
    assert_eq!(changed(&events), [(DepositStatus::Unfinalised, f("0"), Some(AccessReject::EligibilityRequired))]);
    assert!(
        events.iter().any(|e| matches!(e, Event::DepositPending { reason: AccessReject::EligibilityRequired, .. }))
    );
    let events = credit(&mut h, &held, Some(sanctioned), SanctionsStatus::Sanctioned);
    assert!(events.iter().any(|e| matches!(e, Event::DepositHeld { reason: AccessReject::PayerSanctioned, .. })));
    let events = credit(&mut h, &payerless, None, SanctionsStatus::Clear);
    assert!(events.iter().any(|e| matches!(e, Event::DepositHeld { reason: AccessReject::PayerMissing, .. })));
    // A receipt sequenced without screening cannot bypass it: it names no payer, so it is held.
    let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: raw.clone() })));
    assert!(events.iter().any(|e| matches!(e, Event::DepositHeld { reason: AccessReject::PayerMissing, .. })));
    assert_eq!(h.engine.accounts()[&client.account].balance, f("100"), "only the clear receipt moved cash");
    assert_eq!(h.engine.accounts()[&client.account].unfinalised_deposits, f("100"));
    assert!(!h.engine.accounts().contains_key(&newcomer));
    assert_eq!(h.engine.pending_deposits().keys().collect::<Vec<_>>(), [&undeclared.reference()]);
    let holds: Vec<_> = h.engine.held_deposits().keys().cloned().collect();
    assert_eq!(holds, [held.reference(), payerless.reference(), raw.reference()]);
    let recorded = h.engine.recorded_deposit(&held.reference()).unwrap();
    assert_eq!(
        recorded.uncredited,
        Some(UncreditedReceipt { payer: Some(sanctioned), reason: AccessReject::PayerSanctioned })
    );
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn an_uncredited_receipt_follows_reversal_reinstatement_replacement_and_finality_without_moving_cash() {
    let (mut h, client) = setup();
    let pending = receipt(1, Some(client.account), "40");
    let held = receipt(2, Some(client.account), "30");
    credit(&mut h, &pending, Some(client.account), SanctionsStatus::Clear);
    credit(&mut h, &held, Some(AccountId([0x5a; 20])), SanctionsStatus::Sanctioned);
    let reversed =
        transition(&mut h, &pending, |receipt, seq| DepositUpdate::Reverse { receipt, observed_at_seq: seq });
    assert_eq!(changed(&reversed), [(DepositStatus::Reversed, f("0"), Some(AccessReject::EligibilityRequired))]);
    assert!(h.engine.pending_deposits().is_empty(), "a reversed receipt is not collateral the vault holds");
    transition(&mut h, &held, |receipt, seq| DepositUpdate::Reverse { receipt, observed_at_seq: seq });
    assert!(h.engine.held_deposits().is_empty());
    // A reinstated receipt keeps its refusal: a hold is never released by a reorganisation.
    transition(&mut h, &held, |receipt, seq| DepositUpdate::Reinstate { receipt, reversed_at_seq: seq });
    assert_eq!(h.engine.held_deposits()[&held.reference()].reason, AccessReject::PayerSanctioned);
    transition(&mut h, &pending, |receipt, seq| DepositUpdate::Reinstate { receipt, reversed_at_seq: seq });
    assert!(h.engine.pending_deposits().contains_key(&pending.reference()));
    // The replacement is the same funded deposit in another block, so it keeps the refusal.
    let moved = DepositReceipt { block: 110, block_hash: B256::repeat_byte(0x77), ..pending.clone() };
    let events = transition(&mut h, &pending, |receipt, seq| DepositUpdate::Replace {
        receipt,
        observed_at_seq: seq,
        replacement: moved.clone(),
        replacement_reversed_at_seq: None,
    });
    assert_eq!(
        changed(&events),
        [
            (DepositStatus::Reversed, f("0"), Some(AccessReject::EligibilityRequired)),
            (DepositStatus::Unfinalised, f("0"), Some(AccessReject::EligibilityRequired)),
        ]
    );
    assert_eq!(h.engine.pending_deposits().keys().collect::<Vec<_>>(), [&moved.reference()]);
    assert_eq!(h.engine.pending_deposits()[&moved.reference()].payer, Some(client.account));
    for receipt in [&moved, &held] {
        let events =
            transition(&mut h, receipt, |receipt, seq| DepositUpdate::Finalise { receipt, observed_at_seq: seq });
        assert!(matches!(changed(&events)[..], [(DepositStatus::Finalised, amount, Some(_))] if amount == f("0")));
    }
    let checkpoint = CustodyCheckpoint { block: moved.block, block_hash: moved.block_hash, deposit_count: 2 };
    assert!(h.cmd(Command::CustodyFinalised(checkpoint)).is_empty(), "uncredited receipts settle the frontier");
    assert!(!h.engine.accounts().contains_key(&client.account), "nothing entered cash");
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_retried_receipt_is_locked_until_finality_only_while_unfinalised_and_a_sanction_holds_it() {
    let (mut h, client) = setup();
    let early = receipt(1, Some(client.account), "40");
    let late = receipt(2, Some(client.account), "25");
    let sanctioned = receipt(3, Some(client.account), "10");
    for receipt in [&early, &late, &sanctioned] {
        credit(&mut h, receipt, Some(client.account), SanctionsStatus::Clear);
    }
    attest(&mut h, &client);
    transition(&mut h, &late, |receipt, seq| DepositUpdate::Finalise { receipt, observed_at_seq: seq });
    let capacity = h.engine.withdrawal_cash().expect("V24 activation initializes physical cash");
    let events = retry(&mut h, &early, SanctionsStatus::Clear);
    assert_eq!(changed(&events), [(DepositStatus::Unfinalised, f("40"), None)]);
    assert_eq!(h.engine.accounts()[&client.account].unfinalised_deposits, f("40"), "unfinalised principal is locked");
    assert_eq!(h.engine.withdrawal_cash(), Some(capacity), "an unfinalised credit adds no payout capacity");
    let events = retry(&mut h, &late, SanctionsStatus::Clear);
    assert_eq!(changed(&events), [(DepositStatus::Finalised, f("25"), None)]);
    assert_eq!(h.engine.accounts()[&client.account].balance, f("65"));
    assert_eq!(h.engine.accounts()[&client.account].unfinalised_deposits, f("40"), "final principal is not locked");
    assert_eq!(
        h.engine.withdrawal_cash(),
        Some(capacity + f("25")),
        "a receipt finalised while pending still contributes payout capacity once credited"
    );
    transition(&mut h, &early, |receipt, seq| DepositUpdate::Finalise { receipt, observed_at_seq: seq });
    assert_eq!(h.engine.accounts()[&client.account].unfinalised_deposits, f("0"));
    assert_eq!(h.engine.withdrawal_cash(), Some(capacity + f("65")));
    let events = retry(&mut h, &sanctioned, SanctionsStatus::Sanctioned);
    assert_eq!(changed(&events), [(DepositStatus::Unfinalised, f("0"), Some(AccessReject::PayerSanctioned))]);
    assert!(events.iter().any(|e| matches!(e, Event::DepositHeld { reason: AccessReject::PayerSanctioned, .. })));
    assert!(h.engine.pending_deposits().is_empty());
    assert!(h.engine.held_deposits().contains_key(&sanctioned.reference()));
    let refused = retry(&mut h, &sanctioned, SanctionsStatus::Clear);
    assert_eq!(refused, [Event::AdminRejected { reason: "deposit is not awaiting screening".into() }]);
    // A held receipt's reversal removes only the hold: no cash moves.
    transition(&mut h, &sanctioned, |receipt, seq| DepositUpdate::Reverse { receipt, observed_at_seq: seq });
    assert!(h.engine.held_deposits().is_empty());
    assert_eq!(h.engine.accounts()[&client.account].balance, f("65"));
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn an_outage_or_unknown_classification_leaves_a_receipt_pending_never_held() {
    for (evidence, expected) in
        [(Some(SourceFailure::Unavailable), AccessReject::SourceUnavailable), (None, AccessReject::PayerUnknown)]
    {
        let (mut h, client) = setup();
        attest(&mut h, &client);
        let pending = receipt(1, Some(client.account), "40");
        let events = match evidence {
            Some(reason) => h.cmd(Command::AccessCustodyDeposit {
                receipt: pending.clone(),
                payer: Some(client.account),
                evidence: AccessEvidence::Failed { reason },
            }),
            None => credit(&mut h, &pending, Some(client.account), SanctionsStatus::Unknown),
        };
        assert!(events.iter().any(|e| matches!(e, Event::DepositPending { reason, .. } if *reason == expected)));
        assert!(h.engine.held_deposits().is_empty() && h.engine.pending_deposits().contains_key(&pending.reference()));
    }
}

/// A trader receipt for `account`, or a house receipt, in its own block and transaction.
fn receipt(id: u8, account: Option<AccountId>, amount: &str) -> DepositReceipt {
    DepositReceipt {
        block: 100 + u64::from(id),
        block_hash: B256::repeat_byte(id),
        tx_hash: B256::repeat_byte(0x40 + id),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: u64::from(id),
        beneficiary: account.map_or(DepositBeneficiary::House, |account| DepositBeneficiary::Trader { account }),
        amount: f(amount),
    }
}

/// Sequences `receipt` with fresh evidence giving `payer` the status `status`.
fn credit(h: &mut Harness, receipt: &DepositReceipt, payer: Option<AccountId>, status: SanctionsStatus) -> Vec<Event> {
    let evidence = screened(h, receipt, payer, status);
    h.cmd(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer, evidence })
}

/// Retries a pending receipt with fresh evidence giving its recorded payer the status `status`.
fn retry(h: &mut Harness, receipt: &DepositReceipt, status: SanctionsStatus) -> Vec<Event> {
    let payer = h.engine.recorded_deposit(&receipt.reference()).and_then(|d| d.uncredited).and_then(|u| u.payer);
    let evidence = screened(h, receipt, payer, status);
    h.cmd(Command::RetryDepositScreening { chain_ref: receipt.reference(), evidence })
}

fn screened(
    h: &Harness,
    receipt: &DepositReceipt,
    payer: Option<AccountId>,
    status: SanctionsStatus,
) -> AccessEvidence {
    let deposit = receipt_access_deposit(receipt, payer);
    let mut evidence = evidence(h, eip712::deposit_screening_hash(&deposit), deposit.account, payer);
    let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!() };
    screening.payer_status = status;
    sign_screening(h, screening);
    evidence
}

/// Sequences the lifecycle `update` that `make` builds from the receipt and its recorded revision.
fn transition(
    h: &mut Harness,
    receipt: &DepositReceipt,
    make: impl FnOnce(DepositReceipt, u64) -> DepositUpdate,
) -> Vec<Event> {
    let seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    let events = h.cmd(Command::CustodyDeposit(Box::new(make(receipt.clone(), seq))));
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    events
}

/// The status, principal movement and refusal of every receipt change among `events`.
fn changed(events: &[Event]) -> Vec<(DepositStatus, Fixed, Option<AccessReject>)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::CustodyDepositChanged { status, principal_delta, uncredited, .. } => {
                Some((status.clone(), *principal_delta, *uncredited))
            }
            _ => None,
        })
        .collect()
}

/// The incremental state root equals a rebuild's.
fn assert_root_matches_rebuild(engine: &mut Engine) {
    let incremental = engine.state_root();
    let mut rebuilt: Engine = serde_json::from_slice(&serde_json::to_vec(engine).unwrap()).unwrap();
    assert_eq!(rebuilt.state_root(), incremental);
}

#[path = "receipts_v26.rs"]
mod v26;
