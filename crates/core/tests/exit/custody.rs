//! The exit terms and pending deposit leaves over custody receipts: the vault deposit cursor counts
//! every live receipt, and the pending and held totals follow each receipt's disposition.

use alloy_primitives::B256;
use engine_core::access::{AccessEvidence, SanctionsStatus, receipt_access_deposit};
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositStatus, DepositUpdate};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::merkle::{ExitTerms, MAX_EXIT_GAPS, PendingReceipt};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};

use crate::access_support::{attest, evidence, setup, sign_screening};
use crate::common::{Client, Harness, f};

/// A funded and declared client, an undeclared newcomer, and a sanctioned payer, under `V36`, with
/// four receipts: one credited to the client, one pending the newcomer's declaration, one held for
/// its sanctioned payer, and one for the house pending an unknown classification of its payer.
pub fn custody() -> (Harness, [AccountId; 3], [DepositReceipt; 4]) {
    let (mut h, client) = setup();
    attest(&mut h, &client);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V36 });
    let newcomer = Client::new(2).account;
    let sanctioned = AccountId([0x5a; 20]);
    let receipts = [
        receipt(0, Some(client.account), "100"),
        receipt(1, Some(newcomer), "40"),
        receipt(2, Some(client.account), "30"),
        receipt(3, None, "12"),
    ];
    let payers = [
        (client.account, SanctionsStatus::Clear),
        (newcomer, SanctionsStatus::Clear),
        (sanctioned, SanctionsStatus::Sanctioned),
        (newcomer, SanctionsStatus::Unknown),
    ];
    for (receipt, (payer, status)) in receipts.iter().zip(payers) {
        let events = credit(&mut h, receipt, Some(payer), status);
        assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    }
    assert_eq!(h.engine.accounts()[&client.account].balance, f("100"));
    assert_eq!(h.engine.pending_deposits().len(), 2);
    assert_eq!(h.engine.held_deposits().len(), 1);
    (h, [client.account, newcomer, sanctioned], receipts)
}

#[test]
fn the_exit_terms_count_every_live_receipt_and_total_the_pending_and_held_principal() {
    let (mut h, [client, newcomer, _], receipts) = custody();
    let root = B256::from(h.engine.state_root());
    let proof = h.engine.exit_proof(client).unwrap();
    assert_eq!(
        proof.terms.terms,
        ExitTerms { deposit_cursor: 4, pending: f("52"), held: f("30"), unheld: Fixed::ZERO, gaps: vec![] }
    );
    assert!(proof.terms.verify(root));
    assert!(proof.pending_deposits.is_empty(), "the client's own deposits are credited or held");

    let [pending] = &h.engine.exit_proof(newcomer).unwrap().pending_deposits[..] else { panic!("one pending") };
    let expected = PendingReceipt { deposit_id: 1, account: Some(newcomer), payer: Some(newcomer), amount: f("40") };
    assert_eq!((pending.chain_ref.as_str(), pending.deposit), (receipts[1].reference().as_str(), expected));
    assert!(pending.verify(root));
    let forged = engine_core::merkle::PendingDepositProof {
        deposit: PendingReceipt { amount: f("41"), ..expected },
        ..pending.clone()
    };
    assert!(!forged.verify(root), "a pending proof binds its amount");
    let [house] = &h.engine.house_pending_deposit_proofs().unwrap()[..] else { panic!("one house deposit") };
    assert_eq!((house.deposit.deposit_id, house.deposit.account, house.deposit.amount), (3, None, f("12")));
    assert!(house.verify(root));
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_credited_retry_a_reversal_and_a_reinstatement_move_the_totals_and_the_cursor_with_them() {
    let (mut h, [client, newcomer, _], receipts) = custody();
    // A root before the changes, so the next one moves the committed totals rather than rebuilding them.
    h.engine.state_root();
    assert_eq!(h.engine.exit_proof(newcomer).unwrap().pending_deposits.len(), 1);
    attest(&mut h, &Client::new(2));
    let events = retry(&mut h, &receipts[1], SanctionsStatus::Clear);
    assert!(events.iter().any(|e| matches!(e, Event::CustodyDepositChanged { .. })), "{events:?}");
    assert_eq!(h.engine.accounts()[&newcomer].balance, f("40"));
    h.engine.state_root();
    let terms = h.engine.exit_proof(client).unwrap().terms.terms;
    assert_eq!(
        terms,
        ExitTerms { deposit_cursor: 4, pending: f("12"), held: f("30"), unheld: Fixed::ZERO, gaps: vec![] }
    );
    assert!(h.engine.exit_proof(newcomer).unwrap().pending_deposits.is_empty());

    // A reorganisation drops the held receipt: the root no longer holds id 2, a gap below the
    // cursor the exit refunds.
    let seq = h.engine.recorded_deposit(&receipts[2].reference()).unwrap().updated_at_seq;
    let reverse = DepositUpdate::Reverse { receipt: receipts[2].clone(), observed_at_seq: seq };
    let events = h.cmd(Command::CustodyDeposit(Box::new(reverse)));
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    let root = B256::from(h.engine.state_root());
    let proof = h.engine.exit_proof(client).unwrap();
    assert_eq!(
        proof.terms.terms,
        ExitTerms { deposit_cursor: 4, pending: f("12"), held: Fixed::ZERO, unheld: f("30"), gaps: vec![2] }
    );
    assert!(proof.terms.verify(root));

    // The receipt reappears in its block: held again, and counted again.
    let reversed_at_seq = h.engine.recorded_deposit(&receipts[2].reference()).unwrap().updated_at_seq;
    let reinstate = DepositUpdate::Reinstate { receipt: receipts[2].clone(), reversed_at_seq };
    let events = h.cmd(Command::CustodyDeposit(Box::new(reinstate)));
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    assert!(h.engine.held_deposits().contains_key(&receipts[2].reference()));
    h.engine.state_root();
    let terms = h.engine.exit_proof(client).unwrap().terms.terms;
    assert_eq!(
        terms,
        ExitTerms { deposit_cursor: 4, pending: f("12"), held: f("30"), unheld: Fixed::ZERO, gaps: vec![] }
    );
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn the_cursor_counts_the_finalised_frontiers_deposits_and_each_live_receipt_above_it() {
    let (mut h, [client, ..], receipts) = custody();
    let seq = h.engine.recorded_deposit(&receipts[0].reference()).unwrap().updated_at_seq;
    let finalise = DepositUpdate::Finalise { receipt: receipts[0].clone(), observed_at_seq: seq };
    let events = h.cmd(Command::CustodyDeposit(Box::new(finalise)));
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    let frontier = CustodyCheckpoint { block: receipts[0].block, block_hash: receipts[0].block_hash, deposit_count: 1 };
    let events = h.cmd(Command::CustodyFinalised(frontier));
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(h.engine.custody_checkpoint(), Some(frontier));
    let root = B256::from(h.engine.state_root());
    let proof = h.engine.exit_proof(client).unwrap().terms;
    assert_eq!(
        proof.terms,
        ExitTerms { deposit_cursor: 4, pending: f("52"), held: f("30"), unheld: Fixed::ZERO, gaps: vec![] }
    );
    assert!(proof.verify(root));
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_replacement_sequenced_before_the_deposit_that_took_its_old_id_leaves_that_id_refundable() {
    // A reorganisation moves the client's credited deposit from id 0 to id 4, behind a deposit the
    // custody adapter has not credited yet, which now has id 0.
    let (mut h, [client, ..], receipts) = custody();
    let moved =
        DepositReceipt { block: 200, block_hash: B256::repeat_byte(0x70), deposit_id: 4, ..receipts[0].clone() };
    let observed_at_seq = h.engine.recorded_deposit(&receipts[0].reference()).unwrap().updated_at_seq;
    let update = DepositUpdate::Replace {
        receipt: receipts[0].clone(),
        observed_at_seq,
        replacement: moved.clone(),
        replacement_reversed_at_seq: None,
    };
    let evidence = screened(&h, &moved, Some(client), SanctionsStatus::Clear);
    let events = h.cmd(Command::AccessCustodyReplacement { update: Box::new(update), payer: Some(client), evidence });
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    assert_eq!(h.engine.accounts()[&client].balance, f("100"), "the moved deposit stays credited");
    let root = B256::from(h.engine.state_root());
    let terms = h.engine.exit_proof(client).unwrap().terms;
    // Id 0 is a gap the exit refunds; ids 1 to 4, the moved deposit among them, are in the root.
    assert_eq!(
        terms.terms,
        ExitTerms { deposit_cursor: 5, pending: f("52"), held: f("30"), unheld: receipts[0].amount, gaps: vec![0] }
    );
    assert!(terms.verify(root));
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_live_receipt_at_an_occupied_id_reverses_the_stale_one_so_no_root_holds_both() {
    // A reorganisation swapped two deposits' ids; the replacement for the client's credited deposit
    // arrives after the deposit that took its old id, so the new receipt records at an id the root
    // already holds. The stale receipt reverses as the new one records: one live receipt holds the
    // id, its principal moved with the canonical receipt, and a pending proof can never refund the
    // deposit a claim also proves.
    let (mut h, [client, newcomer, _], receipts) = custody();
    attest(&mut h, &Client::new(2));
    let contested = DepositReceipt { deposit_id: 0, ..receipt(100, Some(newcomer), "100") };
    let events = credit(&mut h, &contested, Some(newcomer), SanctionsStatus::Clear);
    let reversed = events.iter().any(|e| {
        matches!(e, Event::CustodyDepositChanged { status: DepositStatus::Reversed, receipt, .. } if receipt.reference() == receipts[0].reference())
    });
    assert!(reversed, "{events:?}");
    assert!(
        h.engine.accounts().get(&client).is_none_or(|account| account.balance == Fixed::ZERO),
        "the stale credit unwound"
    );
    assert_eq!(h.engine.accounts()[&newcomer].balance, f("100"), "the canonical credit stands");
    assert_eq!(h.engine.recorded_deposit(&receipts[0].reference()).unwrap().status, DepositStatus::Reversed);
    let root = B256::from(h.engine.state_root());
    let terms = h.engine.exit_proof(client).unwrap().terms;
    assert_eq!(
        terms.terms,
        ExitTerms { deposit_cursor: 4, pending: f("52"), held: f("30"), unheld: Fixed::ZERO, gaps: vec![] }
    );
    assert!(terms.verify(root));
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn one_gap_among_many_live_receipts_names_itself_without_walking_them() {
    let (mut h, [client, ..], _) = custody();
    let mut held = Vec::new();
    for id in 4..190u64 {
        let receipt = receipt(id as u8, Some(client), "1");
        let events = credit(&mut h, &receipt, Some(client), SanctionsStatus::Clear);
        assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
        held.push(receipt);
    }
    // One reversal leaves one gap amid five hundred live receipts.
    let seq = h.engine.recorded_deposit(&held[0].reference()).unwrap().updated_at_seq;
    let reverse = DepositUpdate::Reverse { receipt: held[0].clone(), observed_at_seq: seq };
    let events = h.cmd(Command::CustodyDeposit(Box::new(reverse)));
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    let root = B256::from(h.engine.state_root());
    let terms = h.engine.exit_proof(client).unwrap().terms;
    assert_eq!((terms.terms.deposit_cursor, &terms.terms.gaps, terms.terms.unheld), (190, &vec![4], f("1")));
    assert!(terms.verify(root));
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn the_terms_name_only_the_lowest_gaps_so_the_vault_starts_the_exit_in_bounded_gas() {
    let (mut h, [client, ..], _) = custody();
    // A live deposit past them keeps the reversed ids below the cursor: sixty-four reversals leave
    // sixty-four gaps, the terms name the lowest `MAX_EXIT_GAPS`, and `unheld` carries every gap's
    // principal.
    let far = DepositReceipt { deposit_id: 200, ..receipt(100, Some(client), "10") };
    let events = credit(&mut h, &far, Some(client), SanctionsStatus::Clear);
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    let mut extra = Vec::new();
    for id in 4..4 + MAX_EXIT_GAPS as u64 {
        let receipt = receipt(id as u8, Some(client), "10");
        let events = credit(&mut h, &receipt, Some(client), SanctionsStatus::Clear);
        assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
        extra.push(receipt);
    }
    for receipt in &extra {
        let seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
        let reverse = DepositUpdate::Reverse { receipt: receipt.clone(), observed_at_seq: seq };
        let events = h.cmd(Command::CustodyDeposit(Box::new(reverse)));
        assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    }
    let root = B256::from(h.engine.state_root());
    let proof = h.engine.exit_proof(client).unwrap().terms;
    let lowest: Vec<u64> = (4..4 + MAX_EXIT_GAPS as u64).collect();
    assert_eq!((proof.terms.deposit_cursor, &proof.terms.gaps, proof.terms.unheld), (201, &lowest, f("640")));
    assert!(proof.verify(root));
}

/// A trader receipt for `account`, or a house receipt, in its own block and transaction, with vault
/// deposit id `id`.
pub fn receipt(id: u8, account: Option<AccountId>, amount: &str) -> DepositReceipt {
    DepositReceipt {
        block: 100 + u64::from(id),
        block_hash: B256::repeat_byte(id + 1),
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

/// Fresh screening evidence for `receipt` giving `payer` the status `status`.
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

/// The incremental state root, and the exit leaves it commits, equal a rebuild's.
fn assert_root_matches_rebuild(engine: &mut Engine) {
    let incremental = engine.state_root();
    let mut rebuilt: Engine = serde_json::from_slice(&serde_json::to_vec(engine).unwrap()).unwrap();
    assert_eq!(rebuilt.state_root(), incremental);
    let accounts: Vec<AccountId> = engine.accounts().keys().copied().collect();
    for account in accounts {
        assert_eq!(rebuilt.exit_proof(account), engine.exit_proof(account), "{account}");
    }
    assert_eq!(rebuilt.house_pending_deposit_proofs(), engine.house_pending_deposit_proofs());
}
