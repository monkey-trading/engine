//! From `V88`, a receipt in a block above the finalised custody frontier is refused at a vault deposit
//! id below the frontier's deposit count, by a credit, a move or a reorganisation: every such id is a
//! deposit at or below the frontier, so a second receipt there would give the exit two receipt leaves at
//! one id. The custody watcher's ordered reads never send one; before `V88` the engine took it.

mod common;
#[allow(dead_code)]
#[path = "deposit_reorganisation/support.rs"]
mod reorg;
#[allow(dead_code)]
#[path = "staking/support.rs"]
mod support;

use alloy_primitives::B256;
use common::Harness;
use engine_core::command::Command;
use engine_core::deposits::{CustodyCheckpoint, DepositReceipt, DepositReject, DepositUpdate};
use engine_core::protocol::ProtocolVersion;
use reorg::{credit, credited_deposits, moved, moves, receipt, rejected, reorganise, replace, revision};

#[test]
fn a_credit_above_the_frontier_at_an_id_it_counts_is_refused() {
    for (version, refused) in [(ProtocolVersion::V87, false), (ProtocolVersion::V88, true)] {
        let (mut h, [first, _], [_, second_old]) = finalised(version);
        let stray = receipt(1, 9_500, 0xd0, 0xd1, first.account, "70");
        let events = credit(&mut h, &stray, first.account);
        let expected = refused.then_some(DepositReject::ConflictingReceipt);
        assert_eq!(rejected(&events), expected, "{version:?}: {events:?}");
        assert_eq!(h.engine.recorded_deposit(&stray.reference()).is_some(), !refused, "{version:?}");
        assert!(h.engine.recorded_deposit(&second_old.reference()).is_some());
    }
}

#[test]
fn a_move_or_reorganisation_onto_an_id_the_frontier_counts_is_refused() {
    let (mut h, [first, _], _) = finalised(ProtocolVersion::V88);
    // A live receipt above the frontier at the next id, which a reorganisation then shows at id 1.
    let live = receipt(2, 9_600, 0xe0, 0xe1, first.account, "50");
    assert_eq!(rejected(&credit(&mut h, &live, first.account)), None);
    let onto_counted = moved(&live, 1, 0xe2);
    let seen = revision(&h, &live);
    let events = replace(&mut h, &live, seen, &onto_counted, first.account);
    assert_eq!(rejected(&events), Some(DepositReject::ConflictingReceipt), "{events:?}");
    let batch = moves(&h, [(&live, &onto_counted, first.account)]);
    let events = reorganise(&mut h, batch);
    assert_eq!(rejected(&events), Some(DepositReject::ConflictingReceipt), "{events:?}");
    assert_eq!(revision(&h, &live), seen, "the refused moves change nothing");
    assert!(h.engine.recorded_deposit(&onto_counted.reference()).is_none());
    // The id above the frontier's count is the receipt's to move to.
    let onward = moved(&live, 3, 0xe3);
    assert_eq!(rejected(&replace(&mut h, &live, seen, &onward, first.account)), None);
}

/// A reversed receipt whose id a later frontier counts, as when the canonical deposit at the id is final
/// in another block, is not reinstated above the frontier from `V88`.
#[test]
fn a_reinstatement_above_the_frontier_at_an_id_it_counts_is_refused() {
    for (version, refused) in [(ProtocolVersion::V87, false), (ProtocolVersion::V88, true)] {
        let (mut h, _, [first, second]) = credited_deposits(version, false);
        let observed_at_seq = revision(&h, &second);
        let reverse = DepositUpdate::Reverse { receipt: second.clone(), observed_at_seq };
        assert_eq!(rejected(&h.cmd(Command::CustodyDeposit(Box::new(reverse)))), None);
        let reversed_at_seq = revision(&h, &second);
        let observed_at_seq = revision(&h, &first);
        let finalise = DepositUpdate::Finalise { receipt: first.clone(), observed_at_seq };
        assert_eq!(rejected(&h.cmd(Command::CustodyDeposit(Box::new(finalise)))), None);
        let frontier = CustodyCheckpoint { block: first.block, block_hash: B256::repeat_byte(0xef), deposit_count: 2 };
        h.cmd(Command::CustodyFinalised(frontier));
        assert_eq!(h.engine.custody_checkpoint(), Some(frontier));
        let reinstate = DepositUpdate::Reinstate { receipt: second.clone(), reversed_at_seq };
        let events = h.cmd(Command::CustodyDeposit(Box::new(reinstate)));
        assert_eq!(rejected(&events), refused.then_some(DepositReject::ConflictingReceipt), "{version:?}: {events:?}");
    }
}

// ── Helpers ──

/// `credited_deposits` at `version`, both receipts final and the frontier at the second's block,
/// counting both.
fn finalised(version: ProtocolVersion) -> (Harness, [common::Client; 2], [DepositReceipt; 2]) {
    let (mut h, clients, receipts) = credited_deposits(version, false);
    for receipt in &receipts {
        let observed_at_seq = revision(&h, receipt);
        let update = DepositUpdate::Finalise { receipt: receipt.clone(), observed_at_seq };
        assert_eq!(rejected(&h.cmd(Command::CustodyDeposit(Box::new(update)))), None);
    }
    let frontier =
        CustodyCheckpoint { block: receipts[1].block, block_hash: B256::repeat_byte(0xee), deposit_count: 2 };
    h.cmd(Command::CustodyFinalised(frontier));
    assert_eq!(h.engine.custody_checkpoint(), Some(frontier));
    (h, clients, receipts)
}
