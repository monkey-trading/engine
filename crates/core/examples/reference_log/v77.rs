//! The session's `V77` part: the state root commits each credited receipt whose principal still awaits
//! finality as an unfinalised credit leaf, through which alone the vault's exit pays that principal. A
//! vote activates the version, which commits every credit already credited. A client's receipt credited
//! on confirmation proves its deposit id, account, payer and amount; a house receipt holds a leaf with no
//! account, found by its id; a receipt whose payer is sanctioned is held, its leaf in the same form.
//! Finality removes the client's leaf and a reversal the house's.

use alloy_primitives::B256;
use engine_core::access::{AccessEvidence, SanctionsStatus, receipt_access_deposit};
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::merkle::{PendingReceipt, ReceiptKind};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::common::close_spread::{attest, evidence, source};
use crate::common::{Client, Harness, f};
use crate::v56::pass;

/// The seed of the key this part deposits through.
const CLIENT_SEED: u8 = 0x78;
/// The vault deposit id of this part's first receipt, past every earlier part's.
const FIRST_ID: u64 = 7_800_000;

/// Activates `V77` by vote; then the credits described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V77 };
    let events = pass(h, 200, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 200, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V77);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);
    let root = B256::from(h.engine.state_root());
    let accounts: Vec<AccountId> = h.engine.accounts().keys().copied().collect();
    for account in accounts {
        let credits = h.engine.exit_proof(account).expect("V38 proves every account's exit").unfinalised_credits;
        assert!(credits.iter().all(|credit| credit.verify(root) && credit.deposit.account == Some(account)));
    }

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    let mine = receipt(FIRST_ID, DepositBeneficiary::Trader { account: client.account }, "500");
    let house = receipt(FIRST_ID + 1, DepositBeneficiary::House, "25");
    for receipt in [&mine, &house] {
        credit(h, receipt, client.account);
    }
    let root = B256::from(h.engine.state_root());
    let credits = h.engine.exit_proof(client.account).expect("V38 proves every account's exit").unfinalised_credits;
    let [credit] = &credits[..] else { panic!("the client's one credit: {credits:?}") };
    let expected = PendingReceipt {
        deposit_id: FIRST_ID,
        account: Some(client.account),
        payer: Some(client.account),
        amount: f("500"),
    };
    assert_eq!((credit.chain_ref.as_str(), credit.deposit), (mine.reference().as_str(), expected));
    assert!(credit.verify(root));
    let house_credit = h.engine.receipt_proof(FIRST_ID + 1).expect("the house's credit");
    let found = (house_credit.kind, house_credit.deposit.account, house_credit.deposit.amount);
    assert_eq!(found, (ReceiptKind::Credited, None, f("25")));
    assert!(house_credit.verify(root));

    let held = receipt(FIRST_ID + 2, DepositBeneficiary::Trader { account: client.account }, "30");
    hold(h, &held, AccountId([0x78; 20]));
    let root = B256::from(h.engine.state_root());
    let proof = h.engine.receipt_proof(FIRST_ID + 2).expect("the held receipt");
    let expected = PendingReceipt {
        deposit_id: FIRST_ID + 2,
        account: Some(client.account),
        payer: Some(AccountId([0x78; 20])),
        amount: f("30"),
    };
    assert_eq!((proof.kind, proof.deposit), (ReceiptKind::Held, expected));
    assert!(proof.verify(root));

    settle(h, &mine, true);
    settle(h, &house, false);
    h.engine.state_root();
    assert!(
        h.engine.exit_proof(client.account).expect("V38 proves every account's exit").unfinalised_credits.is_empty()
    );
    assert_eq!(h.engine.receipt_proof(FIRST_ID), None);
    assert_eq!(h.engine.receipt_proof(FIRST_ID + 1), None);
}

/// A receipt for `beneficiary` at vault deposit id `id`, in a block of its own.
fn receipt(id: u64, beneficiary: DepositBeneficiary, amount: &str) -> DepositReceipt {
    let byte = u8::try_from(id - FIRST_ID).expect("a few receipts") + 0x78;
    DepositReceipt {
        block: id,
        block_hash: B256::repeat_byte(byte),
        tx_hash: B256::repeat_byte(byte.wrapping_add(0x10)),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary,
        amount: f(amount),
    }
}

/// Credits `receipt`, paid by `payer`, on screening evidence that clears it.
fn credit(h: &mut Harness, receipt: &DepositReceipt, payer: AccountId) {
    let deposit = receipt_access_deposit(receipt, Some(payer));
    let evidence = evidence(h, eip712::deposit_screening_hash(&deposit), deposit.account, Some(payer));
    let command = Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: Some(payer), evidence };
    let events = h.cmd(command);
    assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { .. })), "{events:?}");
}

/// Sequences `receipt`, paid by `payer`, on screening evidence that names the payer sanctioned, so it
/// is held.
fn hold(h: &mut Harness, receipt: &DepositReceipt, payer: AccountId) {
    let deposit = receipt_access_deposit(receipt, Some(payer));
    let mut evidence = evidence(h, eip712::deposit_screening_hash(&deposit), deposit.account, Some(payer));
    let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!("screened evidence") };
    screening.payer_status = SanctionsStatus::Sanctioned;
    let domain = h.engine.access_domain().expect("access is recorded");
    screening.signature = eip712::sign(&source(), &eip712::screening_hash(&domain, screening));
    let events = h.cmd(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: Some(payer), evidence });
    assert!(events.iter().any(|event| matches!(event, Event::DepositHeld { .. })), "{events:?}");
}

/// Finalises `receipt` where `finalise`, and reverses it otherwise.
fn settle(h: &mut Harness, receipt: &DepositReceipt, finalise: bool) {
    let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).expect("recorded").updated_at_seq;
    let receipt = receipt.clone();
    let update = if finalise {
        DepositUpdate::Finalise { receipt, observed_at_seq }
    } else {
        DepositUpdate::Reverse { receipt, observed_at_seq }
    };
    let events = h.cmd(Command::CustodyDeposit(Box::new(update)));
    assert!(!events.iter().any(|event| matches!(event, Event::CustodyDepositRejected { .. })), "{events:?}");
}
