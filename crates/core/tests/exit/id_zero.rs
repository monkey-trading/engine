//! The exit terms over the smallest custody state: the frontier at zero and no live receipt.

use alloy_primitives::B256;
use engine_core::Fixed;
use engine_core::access::SanctionsStatus;
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::DepositUpdate;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;

use crate::access_support::{attest, setup};
use crate::custody::{assert_root_matches_rebuild, credit, receipt};

#[test]
fn a_sole_reversed_deposit_at_id_zero_reserves_nothing_past_the_cursor() {
    // With the frontier at zero and no live receipt, reversing the deposit at id 0 left its
    // principal in `unheld` although the cursor stays at zero: the vault's cursor rule refunds
    // id 0 by id, so `startExit` reserved the principal twice. From V39 the empty live index
    // excludes every missing id; a V38 log keeps its committed terms (see the gated exclusion).
    let (mut h, client) = setup();
    attest(&mut h, &client);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V39 });
    let receipt = receipt(0, Some(client.account), "50");
    credit(&mut h, &receipt, Some(client.account), SanctionsStatus::Clear);
    let seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse {
        receipt: receipt.clone(),
        observed_at_seq: seq,
    })));
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    assert_eq!(h.engine.accounts()[&client.account].balance, Fixed::ZERO, "the reversal refunds the credit");
    let root = B256::from(h.engine.state_root());
    let terms = h.engine.exit_proof(client.account).unwrap().terms;
    assert_eq!(
        (terms.terms.deposit_cursor, &terms.terms.gaps, terms.terms.unheld, terms.terms.held_receipts),
        (0, &vec![], Fixed::ZERO, Fixed::ZERO),
        "no id sits below the empty cursor, so nothing is unheld"
    );
    assert!(terms.verify(root));
    assert_root_matches_rebuild(&mut h.engine);
}
