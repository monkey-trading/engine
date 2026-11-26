//! `V91`'s deposit record leaves through `Engine::apply`: each pending and held deposit's record has a leaf of its
//! own, written at the activation for every deposit then pending or held and as one becomes pending or held, and
//! removed with it, so every root is the one rebuilt from the encoding, while the exit's leaves of those deposits keep
//! the form the vault checks.

use alloy_primitives::B256;
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::DepositUpdate;
use engine_core::event::Event;
use engine_core::merkle::ReceiptKind;
use engine_core::protocol::ProtocolVersion;

use crate::common::Harness;
use crate::custody::{assert_root_matches_rebuild, custody_at};

#[test]
fn each_record_leaf_follows_its_deposit_from_the_activation() {
    // Receipts 1 and 3 pending and 2 held under `V90`, whose roots leave their records out.
    let (mut h, _, receipts) = custody_at(ProtocolVersion::V90);
    h.engine.state_root();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V91 });
    assert_root_matches_rebuild(&mut h.engine);

    // A reorganisation drops a pending receipt and the held one, and each record goes with it.
    for receipt in [&receipts[3], &receipts[2]] {
        let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
        settle(&mut h, DepositUpdate::Reverse { receipt: receipt.clone(), observed_at_seq });
        assert_root_matches_rebuild(&mut h.engine);
    }
    assert_eq!((h.engine.pending_deposits().len(), h.engine.held_deposits().len()), (1, 0));

    // Back in the canonical chain, the held receipt is held again, its record written afresh.
    let reversed_at_seq = h.engine.recorded_deposit(&receipts[2].reference()).unwrap().updated_at_seq;
    settle(&mut h, DepositUpdate::Reinstate { receipt: receipts[2].clone(), reversed_at_seq });
    assert!(h.engine.held_deposits().contains_key(&receipts[2].reference()));
    assert_root_matches_rebuild(&mut h.engine);
    let root = B256::from(h.engine.state_root());
    let held = h.engine.receipt_proof(receipts[2].deposit_id).expect("the held receipt's exit leaf");
    assert!(held.kind == ReceiptKind::Held && held.verify(root), "{held:?}");
}

// ── Helpers ──

/// Applies the custody watcher's `update`, which must not be refused.
fn settle(h: &mut Harness, update: DepositUpdate) {
    let events = h.cmd(Command::CustodyDeposit(Box::new(update)));
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
}
