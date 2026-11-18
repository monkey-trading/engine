//! `V77`'s unfinalised credit leaves through `Engine::apply`: each credited receipt still awaiting
//! finality holds one, binding the deposit id, beneficiary, payer and amount the vault's own records
//! must match, from its credit until it finalises, reverses or is replaced, and a receipt-backed held
//! deposit's leaf binds the same; an activation commits those already credited or held, and earlier
//! versions commit neither. At every root an account's credits sum to its leaf's unfinalised
//! principal, which the vault takes out of its claim and adds back credit by credit.

use alloy_primitives::{B256, keccak256};
use engine_core::Fixed;
use engine_core::access::SanctionsStatus;
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::{DepositReceipt, DepositUpdate};
use engine_core::event::Event;
use engine_core::merkle::{PendingReceipt, ReceiptKind, ReceiptProof, UnfinalisedCreditProof, unfinalised_credit_key};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::access_support::attest;
use crate::common::close_spread::{ceilings, install_expect};
use crate::common::{Client, Harness, f};
use crate::custody::{assert_root_matches_rebuild, credit, custody, custody_at, receipt, retry, screened};

const V77: ProtocolVersion = ProtocolVersion::V77;

#[test]
fn a_credited_receipt_holds_its_leaf_until_it_finalises() {
    let (mut h, [client, ..], receipts) = custody_at(V77);
    let root = B256::from(h.engine.state_root());
    let credits = h.engine.exit_proof(client).unwrap().unfinalised_credits;
    let [credit] = &credits[..] else { panic!("one credit: {credits:?}") };
    let expected = PendingReceipt { deposit_id: 0, account: Some(client), payer: Some(client), amount: f("100") };
    assert_eq!((credit.chain_ref.as_str(), credit.deposit), (receipts[0].reference().as_str(), expected));
    assert_eq!(credit.deposit.amount, h.engine.accounts()[&client].unfinalised_deposits, "its account's lock");
    assert!(credit.verify(root));
    assert_credits_make_up_the_lock(&h, client);
    for forged in [PendingReceipt { payer: None, ..expected }, PendingReceipt { deposit_id: 1, ..expected }] {
        assert!(!UnfinalisedCreditProof { deposit: forged, ..credit.clone() }.verify(root), "{forged:?}");
    }

    settle(&mut h, &receipts[0], true);
    h.engine.state_root();
    assert!(h.engine.exit_proof(client).unwrap().unfinalised_credits.is_empty(), "a final receipt holds none");
    assert_credits_make_up_the_lock(&h, client);
    assert_eq!(h.engine.receipt_proof(0), None);
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_pending_receipt_takes_its_leaf_once_screening_credits_it_and_a_reversal_removes_it() {
    let (mut h, [client, newcomer, sanctioned], receipts) = custody_at(V77);
    let root = B256::from(h.engine.state_root());
    assert!(h.engine.exit_proof(newcomer).unwrap().unfinalised_credits.is_empty(), "a pending receipt is no credit");
    assert!(matches!(h.engine.receipt_proof(1), Some(ref p) if p.kind == ReceiptKind::Pending && p.verify(root)));
    let held = h.engine.receipt_proof(2).expect("the held receipt");
    let expected = PendingReceipt { deposit_id: 2, account: Some(client), payer: Some(sanctioned), amount: f("30") };
    assert_eq!((held.kind, held.deposit), (ReceiptKind::Held, expected), "a held receipt's ABI leaf");
    assert!(held.verify(root));

    attest(&mut h, &Client::new(2));
    let events = retry(&mut h, &receipts[1], SanctionsStatus::Clear);
    assert!(events.iter().any(|e| matches!(e, Event::CustodyDepositChanged { .. })), "{events:?}");
    let root = B256::from(h.engine.state_root());
    let credits = h.engine.exit_proof(newcomer).unwrap().unfinalised_credits;
    let [credit] = &credits[..] else { panic!("one credit: {credits:?}") };
    let expected = PendingReceipt { deposit_id: 1, account: Some(newcomer), payer: Some(newcomer), amount: f("40") };
    assert_eq!(credit.deposit, expected);
    let found = h.engine.receipt_proof(1).expect("the credit");
    assert_eq!(
        (found.kind, &found.chain_ref, found.deposit),
        (ReceiptKind::Credited, &credit.chain_ref, credit.deposit)
    );
    assert!(found.verify(root));
    assert_credits_make_up_the_lock(&h, newcomer);
    assert_root_matches_rebuild(&mut h.engine);

    settle(&mut h, &receipts[1], false);
    h.engine.state_root();
    assert!(h.engine.exit_proof(newcomer).unwrap().unfinalised_credits.is_empty(), "a reversed receipt holds none");
    assert_credits_make_up_the_lock(&h, newcomer);
    assert_eq!(h.engine.receipt_proof(1), None);
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_replacement_moves_the_leaf_to_the_receipts_new_id() {
    let (mut h, [client, ..], receipts) = custody_at(V77);
    h.engine.state_root();
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
    let root = B256::from(h.engine.state_root());
    let credits = h.engine.exit_proof(client).unwrap().unfinalised_credits;
    let [credit] = &credits[..] else { panic!("one credit: {credits:?}") };
    assert_eq!((credit.chain_ref.as_str(), credit.deposit.deposit_id), (moved.reference().as_str(), 4));
    assert!(credit.verify(root));
    assert_eq!(h.engine.receipt_proof(0), None, "the orphan's id holds no receipt");
    assert_credits_make_up_the_lock(&h, client);
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_held_receipt_replaced_under_another_payer_binds_the_replacements_payer() {
    // The replacement keeps its transaction's terminal hold, screened for the orphan's payer, while
    // the vault records the replacement's own caller, which its leaf must name to match.
    let (mut h, _, receipts) = custody_at(V77);
    h.engine.state_root();
    let moved =
        DepositReceipt { block: 210, block_hash: B256::repeat_byte(0x71), deposit_id: 5, ..receipts[2].clone() };
    let observed_at_seq = h.engine.recorded_deposit(&receipts[2].reference()).unwrap().updated_at_seq;
    let update = DepositUpdate::Replace {
        receipt: receipts[2].clone(),
        observed_at_seq,
        replacement: moved.clone(),
        replacement_reversed_at_seq: None,
    };
    let relayer = AccountId([0x6b; 20]);
    let evidence = screened(&h, &moved, Some(relayer), SanctionsStatus::Clear);
    let events = h.cmd(Command::AccessCustodyReplacement { update: Box::new(update), payer: Some(relayer), evidence });
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    assert!(h.engine.held_deposits().contains_key(&moved.reference()), "the terminal hold follows the transaction");
    let root = B256::from(h.engine.state_root());
    let held = h.engine.receipt_proof(5).expect("the replacement's held leaf");
    assert_eq!((held.kind, held.deposit.deposit_id, held.deposit.payer), (ReceiptKind::Held, 5, Some(relayer)));
    assert!(held.verify(root));
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_payerless_holds_leaf_names_no_payer_as_the_vault_records_a_surplus() {
    // A deposit with no funding address, as a booked surplus is, is held for good; its leaf names
    // no payer and no account, which the vault's record of a surplus matches, so it stays held,
    // while an orphaned one, which the vault does not record, can be released.
    let (mut h, ..) = custody_at(V77);
    let payerless = receipt(5, None, "20");
    let events = credit(&mut h, &payerless, None, SanctionsStatus::Clear);
    assert!(events.iter().any(|e| matches!(e, Event::DepositHeld { .. })), "{events:?}");
    let root = B256::from(h.engine.state_root());
    assert!(h.engine.held_deposits().contains_key(&payerless.reference()));
    let held = h.engine.receipt_proof(5).expect("the payerless hold's leaf");
    let expected = PendingReceipt { deposit_id: 5, account: None, payer: None, amount: f("20") };
    assert_eq!((held.kind, held.deposit), (ReceiptKind::Held, expected));
    assert!(held.verify(root));
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_held_replacement_recording_no_payer_under_a_hold_that_names_one_keeps_its_json_leaf() {
    // The replacement records no vault caller while its terminal hold names the orphan's: with no
    // caller to bind, the exit cannot prove it unmatched, and it stays held.
    let (mut h, _, receipts) = custody_at(V77);
    h.engine.state_root();
    let moved =
        DepositReceipt { block: 220, block_hash: B256::repeat_byte(0x72), deposit_id: 6, ..receipts[2].clone() };
    let observed_at_seq = h.engine.recorded_deposit(&receipts[2].reference()).unwrap().updated_at_seq;
    let update = DepositUpdate::Replace {
        receipt: receipts[2].clone(),
        observed_at_seq,
        replacement: moved.clone(),
        replacement_reversed_at_seq: None,
    };
    let evidence = screened(&h, &moved, None, SanctionsStatus::Clear);
    let events = h.cmd(Command::AccessCustodyReplacement { update: Box::new(update), payer: None, evidence });
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    h.engine.state_root();
    assert!(h.engine.held_deposits().contains_key(&moved.reference()), "the terminal hold follows the transaction");
    assert_eq!(h.engine.recorded_deposit(&moved.reference()).unwrap().payer, None);
    assert_eq!(h.engine.receipt_proof(6), None);
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_house_credit_holds_a_leaf_with_no_account_found_by_its_id() {
    let (mut h, ..) = custody_at(V77);
    let payer = AccountId([0x77; 20]);
    let house = receipt(4, None, "25");
    let events = credit(&mut h, &house, Some(payer), SanctionsStatus::Clear);
    assert!(events.iter().any(|e| matches!(e, Event::CustodyDepositChanged { .. })), "{events:?}");
    let root = B256::from(h.engine.state_root());
    let proof = h.engine.receipt_proof(4).expect("the house's credit");
    let expected = PendingReceipt { deposit_id: 4, account: None, payer: Some(payer), amount: f("25") };
    assert_eq!(
        (proof.kind, proof.chain_ref.clone(), proof.deposit),
        (ReceiptKind::Credited, house.reference(), expected)
    );
    assert!(proof.verify(root));
    let forged = ReceiptProof { kind: ReceiptKind::Pending, ..proof };
    assert!(!forged.verify(root), "a proof binds its leaf's kind");
    assert!(
        matches!(h.engine.receipt_proof(3), Some(ref p) if p.kind == ReceiptKind::Pending && p.deposit.account.is_none())
    );
    assert_eq!(h.engine.receipt_proof(9), None);
}

#[test]
fn activation_commits_the_credits_already_credited_and_earlier_versions_commit_none() {
    let (mut h, [client, ..], receipts) = custody();
    let before = B256::from(h.engine.state_root());
    assert!(h.engine.exit_proof(client).unwrap().unfinalised_credits.is_empty());
    assert_eq!(h.engine.receipt_proof(0), None, "V38 holds no credit leaf");
    assert_eq!(h.engine.receipt_proof(2), None, "nor an ABI held leaf");
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: V77 });
    assert_eq!(h.engine.receipt_proof(2), None, "the last root still holds the held receipt's JSON leaf");
    let root = B256::from(h.engine.state_root());
    assert_ne!(root, before);
    let credits = h.engine.exit_proof(client).unwrap().unfinalised_credits;
    let [credit] = &credits[..] else { panic!("one credit: {credits:?}") };
    assert_eq!(credit.chain_ref, receipts[0].reference());
    assert!(credit.verify(root));
    assert_credits_make_up_the_lock(&h, client);
    let held = h.engine.receipt_proof(2).expect("the held receipt's leaf rehashed");
    assert!(held.kind == ReceiptKind::Held && held.verify(root));
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_reinstated_receipt_takes_its_leaf_back() {
    let (mut h, [client, ..], receipts) = custody_at(V77);
    settle(&mut h, &receipts[0], false);
    h.engine.state_root();
    assert!(h.engine.exit_proof(client).unwrap().unfinalised_credits.is_empty());
    let reversed_at_seq = h.engine.recorded_deposit(&receipts[0].reference()).unwrap().updated_at_seq;
    let reinstate = DepositUpdate::Reinstate { receipt: receipts[0].clone(), reversed_at_seq };
    let events = h.cmd(Command::CustodyDeposit(Box::new(reinstate)));
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    let root = B256::from(h.engine.state_root());
    let credits = h.engine.exit_proof(client).unwrap().unfinalised_credits;
    assert!(matches!(&credits[..], [credit] if credit.chain_ref == receipts[0].reference() && credit.verify(root)));
    assert_credits_make_up_the_lock(&h, client);
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_receipt_that_takes_a_credited_receipts_id_replaces_its_leaf() {
    // The conflict sweep reverses the stale credit at id 0 as the canonical receipt there records.
    let (mut h, [client, ..], receipts) = custody_at(V77);
    h.engine.state_root();
    let canonical = DepositReceipt {
        block: 150,
        block_hash: B256::repeat_byte(0x55),
        tx_hash: B256::repeat_byte(0x56),
        amount: f("70"),
        ..receipts[0].clone()
    };
    let events = credit(&mut h, &canonical, Some(client), SanctionsStatus::Clear);
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    let root = B256::from(h.engine.state_root());
    let credits = h.engine.exit_proof(client).unwrap().unfinalised_credits;
    let [credit] = &credits[..] else { panic!("one credit: {credits:?}") };
    assert_eq!((credit.chain_ref.as_str(), credit.deposit.amount), (canonical.reference().as_str(), f("70")));
    assert!(credit.verify(root));
    assert_credits_make_up_the_lock(&h, client);
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn a_replacement_screening_leaves_pending_drops_the_credit_for_a_pending_leaf() {
    // The replacement's payer differs from the orphan's and its screening cannot yet classify it.
    let (mut h, [client, newcomer, _], receipts) = custody_at(V77);
    h.engine.state_root();
    let moved =
        DepositReceipt { block: 200, block_hash: B256::repeat_byte(0x70), deposit_id: 4, ..receipts[0].clone() };
    let observed_at_seq = h.engine.recorded_deposit(&receipts[0].reference()).unwrap().updated_at_seq;
    let update = DepositUpdate::Replace {
        receipt: receipts[0].clone(),
        observed_at_seq,
        replacement: moved.clone(),
        replacement_reversed_at_seq: None,
    };
    let evidence = screened(&h, &moved, Some(newcomer), SanctionsStatus::Unknown);
    let events = h.cmd(Command::AccessCustodyReplacement { update: Box::new(update), payer: Some(newcomer), evidence });
    assert!(events.iter().any(|e| matches!(e, Event::DepositPending { .. })), "{events:?}");
    let root = B256::from(h.engine.state_root());
    assert!(h.engine.exit_proof(client).unwrap().unfinalised_credits.is_empty());
    assert_credits_make_up_the_lock(&h, client);
    let pending = h.engine.receipt_proof(4).expect("the replacement's pending leaf");
    assert!(pending.kind == ReceiptKind::Pending && pending.verify(root));
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn the_credit_leaf_is_keyed_by_kind_35_and_its_reference_as_the_vault_keys_it() {
    // `keccak256(abi.encodePacked(uint8(35), bytes(chainRef)))`, the vault's `UNFINALISED_CREDIT_LEAF`.
    let reference = "00000000000000000100:0x01:0x40:0";
    assert_eq!(unfinalised_credit_key(reference), keccak256([&[35u8][..], reference.as_bytes()].concat()));
}

/// `account`'s committed credits sum to the unfinalised principal its committed leaf holds.
fn assert_credits_make_up_the_lock(h: &Harness, account: AccountId) {
    let exit = h.engine.exit_proof(account).unwrap();
    let locked = exit.account.map_or(Fixed::ZERO, |proof| proof.leaf.unfinalised_deposits);
    let credited: Fixed = exit.unfinalised_credits.iter().map(|credit| credit.deposit.amount).sum();
    assert_eq!(credited, locked, "{account}");
}

/// Finalises `receipt` where `finalise`, and reverses it otherwise, at its recorded revision.
fn settle(h: &mut Harness, receipt: &DepositReceipt, finalise: bool) {
    let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    let receipt = receipt.clone();
    let update = if finalise {
        DepositUpdate::Finalise { receipt, observed_at_seq }
    } else {
        DepositUpdate::Reverse { receipt, observed_at_seq }
    };
    let events = h.cmd(Command::CustodyDeposit(Box::new(update)));
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
}
