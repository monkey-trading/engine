//! V26 payer binding and transaction-local terminal holds across canonical receipts.

use super::*;
use engine_core::command::AdminCommand;
use engine_core::protocol::ProtocolVersion;

#[test]
fn canonical_replacement_payers_are_screened_atomically_and_authenticated() {
    for (status, credited) in
        [(SanctionsStatus::Clear, true), (SanctionsStatus::Sanctioned, false), (SanctionsStatus::Unknown, false)]
    {
        let (mut h, client) = setup();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V26 });
        attest(&mut h, &client);
        let first = receipt(1, Some(client.account), "100");
        credit(&mut h, &first, Some(client.account), SanctionsStatus::Clear);
        assert_eq!(h.engine.recorded_deposit(&first.reference()).unwrap().payer, Some(client.account));
        let moved = DepositReceipt { block: 110, block_hash: B256::repeat_byte(0x77), ..first.clone() };
        let payer = Some(AccountId([0x5a; 20]));
        let events = replace(&mut h, &first, &moved, payer, status);
        assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
        assert_eq!(h.engine.accounts()[&client.account].balance, if credited { f("100") } else { f("0") });
        let stored = h.engine.recorded_deposit(&moved.reference()).unwrap();
        assert_eq!(stored.payer, payer);
        assert_eq!(stored.uncredited.is_none(), credited);
        assert_eq!(h.engine.recorded_deposit(&first.reference()).unwrap().status, DepositStatus::Reversed);
        let root = h.engine.state_root();
        let mut encoded = serde_json::to_value(&h.engine).unwrap();
        encoded["deposits"][moved.reference()]["payer"] = serde_json::to_value(client.account).unwrap();
        let mut changed: Engine = serde_json::from_value(encoded).unwrap();
        assert_ne!(changed.state_root(), root, "the canonical payer is authenticated");
        assert_root_matches_rebuild(&mut h.engine);
    }
}

#[test]
fn terminal_holds_survive_disappearance_reinclusion_restart_and_return_to_an_earlier_receipt() {
    for account in [false, true] {
        let (mut h, client) = setup();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V26 });
        attest(&mut h, &client);
        let house_before = h.engine.house_cash();
        let first = receipt(1, account.then_some(client.account), "100");
        let payer = Some(AccountId([0x5a; 20]));
        credit(&mut h, &first, payer, SanctionsStatus::Sanctioned);
        transition(&mut h, &first, |receipt, observed_at_seq| DepositUpdate::Reverse { receipt, observed_at_seq });
        h.engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
        let moved = DepositReceipt { block: 110, block_hash: B256::repeat_byte(0x77), ..first.clone() };
        credit(&mut h, &moved, Some(client.account), SanctionsStatus::Clear);
        assert_eq!(h.engine.held_deposits()[&moved.reference()].reason, AccessReject::PayerSanctioned);
        replace(&mut h, &moved, &first, Some(client.account), SanctionsStatus::Clear);
        assert_eq!(h.engine.held_deposits()[&first.reference()].reason, AccessReject::PayerSanctioned);
        assert_eq!(h.engine.recorded_deposit(&moved.reference()).unwrap().status, DepositStatus::Reversed);
        assert_eq!(h.engine.accounts().get(&client.account).map_or(f("0"), |account| account.balance), f("0"));
        assert_eq!(h.engine.house_cash(), house_before);
        assert_root_matches_rebuild(&mut h.engine);
    }
}

#[test]
fn bare_replacement_updates_cannot_preserve_a_credit_without_canonical_screening() {
    let (mut h, client) = setup();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V26 });
    attest(&mut h, &client);
    let first = receipt(1, Some(client.account), "100");
    credit(&mut h, &first, Some(client.account), SanctionsStatus::Clear);
    let moved = DepositReceipt { block: 110, block_hash: B256::repeat_byte(0x77), ..first.clone() };
    transition(&mut h, &first, |receipt, observed_at_seq| DepositUpdate::Replace {
        receipt,
        observed_at_seq,
        replacement: moved.clone(),
        replacement_reversed_at_seq: None,
    });
    assert_eq!(h.engine.accounts()[&client.account].balance, f("0"));
    assert_eq!(h.engine.held_deposits()[&moved.reference()].reason, AccessReject::PayerMissing);
}

#[test]
fn activation_restores_terminal_holds_hidden_by_legacy_replacement_history() {
    let (mut h, client) = setup();
    attest(&mut h, &client);
    let original = receipt(1, Some(client.account), "100");
    credit(&mut h, &original, Some(client.account), SanctionsStatus::Sanctioned);
    transition(&mut h, &original, |receipt, observed_at_seq| DepositUpdate::Reverse { receipt, observed_at_seq });
    let moved = DepositReceipt { block: 110, block_hash: B256::repeat_byte(0x77), ..original.clone() };
    credit(&mut h, &moved, Some(client.account), SanctionsStatus::Clear);
    assert_eq!(h.engine.accounts()[&client.account].balance, f("100"), "the committed V24 rule is preserved");
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V26 });
    let last = DepositReceipt { block: 111, block_hash: B256::repeat_byte(0x78), ..moved.clone() };
    replace(&mut h, &moved, &last, Some(client.account), SanctionsStatus::Clear);
    assert_eq!(h.engine.accounts()[&client.account].balance, f("0"));
    assert_eq!(h.engine.held_deposits()[&last.reference()].reason, AccessReject::PayerSanctioned);
    assert_root_matches_rebuild(&mut h.engine);
}

#[test]
fn clear_replacements_keep_solvent_positions_open_without_a_transient_balance_drop() {
    let (mut h, client) = setup();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V26 });
    attest(&mut h, &client);
    let original = receipt(1, Some(client.account), "100");
    credit(&mut h, &original, Some(client.account), SanctionsStatus::Clear);
    let request =
        order(&h, &client, engine_core::command::OrderKind::Market { quoted: f("100"), max_slippage_bps: None });
    let evidence = order_evidence(&h, &request);
    let events = h.cmd(Command::AccessOrder { request, evidence });
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    let position = h.engine.accounts()[&client.account].position(crate::common::BTC);
    assert_ne!(position.qty, f("0"));
    let moved = DepositReceipt { block: 110, block_hash: B256::repeat_byte(0x77), ..original.clone() };
    replace(&mut h, &original, &moved, Some(AccountId([0x5a; 20])), SanctionsStatus::Clear);
    assert_eq!(h.engine.accounts()[&client.account].position(crate::common::BTC), position);
    assert_eq!(h.engine.accounts()[&client.account].balance, f("100"));
}

fn replace(
    h: &mut Harness,
    original: &DepositReceipt,
    replacement: &DepositReceipt,
    payer: Option<AccountId>,
    status: SanctionsStatus,
) -> Vec<Event> {
    let observed_at_seq = h.engine.recorded_deposit(&original.reference()).unwrap().updated_at_seq;
    let replacement_reversed_at_seq =
        h.engine.recorded_deposit(&replacement.reference()).map(|recorded| recorded.updated_at_seq);
    let evidence = screened(h, replacement, payer, status);
    let update = DepositUpdate::Replace {
        receipt: original.clone(),
        observed_at_seq,
        replacement: replacement.clone(),
        replacement_reversed_at_seq,
    };
    h.cmd(Command::AccessCustodyReplacement { update: Box::new(update), payer, evidence })
}
