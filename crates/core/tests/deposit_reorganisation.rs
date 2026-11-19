//! From `V83`, a reorganisation's receipt moves apply as one command (`Command::CustodyReorganisation`):
//! a credited deposit that is canonical again at another vault id moves there with its cash and its
//! position untouched, a receipt reverses only when the chain dropped it, and a write-off a reversal's
//! close-out charged the house is repaid from the same deposit's next credit. Each test drives the
//! commands the custody watcher sequences.

mod common;
#[path = "deposit_reorganisation/support.rs"]
mod reorg;
#[allow(dead_code)]
#[path = "staking/support.rs"]
mod support;

use alloy_primitives::B256;
use common::close_spread::trade;
use common::events::fills;
use common::{BTC, f};
use engine_core::command::Command;
use engine_core::deposits::{DepositReceipt, DepositReject, DepositStatus, DepositUpdate};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;
use reorg::{
    V83, credit, credited_deposits, moved, moves, rejected, reorganise, replace, reversed, revision, total_cash,
};

#[test]
fn a_reorganisation_that_shifts_deposit_ids_keeps_each_canonical_deposit_and_its_position() {
    let (mut h, [first, second], [first_old, second_old]) = credited_deposits(V83, true);
    let equity_before = h.equity_and_margin(second.account).0;
    let (cash_before, balances) = (total_cash(&h), (h.balance(first.account), h.balance(second.account)));
    // Another deposit takes id 0, so both credited deposits are canonical again one id later.
    let (first_new, second_new) = (moved(&first_old, 1, 0xc0), moved(&second_old, 2, 0xc1));
    let batch = moves(&h, [(&first_old, &first_new, first.account), (&second_old, &second_new, second.account)]);
    let events = reorganise(&mut h, batch);
    assert_eq!(rejected(&events), None, "{events:?}");
    assert!(!events.iter().any(|event| matches!(event, Event::CloseOut { .. } | Event::BadDebt { .. })), "{events:?}");
    assert_eq!(reversed(&events), vec![first_old.reference(), second_old.reference()], "only the orphans leave");
    assert_eq!(h.engine.accounts()[&second.account].positions.len(), 1, "a solvent position stays open");
    assert_eq!(h.equity_and_margin(second.account).0, equity_before);
    assert_eq!((h.balance(first.account), h.balance(second.account)), balances);
    assert_eq!(total_cash(&h), cash_before, "the moves net out");
    for (receipt, account) in [(&first_new, first.account), (&second_new, second.account)] {
        let recorded = h.engine.recorded_deposit(&receipt.reference()).expect("recorded");
        assert_eq!((recorded.status.clone(), recorded.payer), (DepositStatus::Unfinalised, Some(account)));
    }
    // The inserted deposit then credits at id 0, which no live receipt holds.
    let inserted = DepositReceipt { tx_hash: B256::repeat_byte(0xb9), ..moved(&first_old, 0, 0xc0) };
    assert_eq!(rejected(&credit(&mut h, &inserted, first.account)), None);
}

#[test]
fn a_reorganisation_that_shifts_ids_leaves_a_flat_clients_realised_loss_with_the_house() {
    let (mut h, [first, second], [first_old, second_old]) = credited_deposits(V83, true);
    let sold = trade(&mut h, &second, BTC, Side::Sell, "0.05", "97000");
    assert_eq!(fills(&sold).len(), 1, "{sold:?}");
    let (cash_before, house_before) = (h.balance(second.account), h.engine.house_cash());
    assert!(cash_before < f("850") && cash_before > f("800"), "{cash_before}");
    let batch = moves(
        &h,
        [
            (&first_old, &moved(&first_old, 1, 0xc0), first.account),
            (&second_old, &moved(&second_old, 2, 0xc1), second.account),
        ],
    );
    assert_eq!(rejected(&reorganise(&mut h, batch)), None);
    assert_eq!(h.balance(second.account), cash_before);
    assert_eq!(h.engine.house_cash(), house_before);
}

#[test]
fn a_move_onto_an_id_another_live_receipt_holds_reverses_nothing() {
    // The moves one at a time, as before `V83`: the first lands on the id the second client's live
    // receipt holds. Before `V83` that reverses the second receipt and closes its position out; from
    // `V83` the move is refused, and neither receipt changes.
    for (version, displaced) in [(ProtocolVersion::V80, true), (V83, false)] {
        let (mut h, [first, second], [first_old, _]) = credited_deposits(version, true);
        let (seen, cash) = (revision(&h, &first_old), total_cash(&h));
        let events = replace(&mut h, &first_old, seen, &moved(&first_old, 1, 0xc0), first.account);
        let closed_out =
            events.iter().any(|event| matches!(event, Event::CloseOut { account, .. } if *account == second.account));
        assert_eq!(closed_out, displaced, "{version:?}: {events:?}");
        if !displaced {
            assert_eq!(rejected(&events), Some(DepositReject::ConflictingReceipt), "{events:?}");
            assert_eq!(h.engine.accounts()[&second.account].positions.len(), 1);
            assert_eq!(total_cash(&h), cash);
            assert_eq!(revision(&h, &first_old), seen, "the refused move changes nothing");
        }
    }
}

#[test]
fn a_reincluded_deposit_alone_moves_in_one_step_and_keeps_its_position() {
    for version in [ProtocolVersion::V80, V83] {
        let (mut h, [_, second], [_, second_old]) = credited_deposits(version, true);
        let equity_before = h.equity_and_margin(second.account).0;
        let second_new = DepositReceipt { block: 9_011, block_hash: B256::repeat_byte(0xc1), ..second_old.clone() };
        let seen = revision(&h, &second_old);
        assert_eq!(rejected(&replace(&mut h, &second_old, seen, &second_new, second.account)), None);
        assert_eq!(h.engine.accounts()[&second.account].positions.len(), 1, "{version:?}");
        assert_eq!(h.equity_and_margin(second.account).0, equity_before);
    }
}

#[test]
fn a_reorganisation_that_drops_a_deposit_reverses_only_that_one() {
    // The first deposit leaves the chain for good, and the second, still canonical, takes its id. The
    // watcher reverses the dropped receipt first, then sequences the move.
    let (mut h, [first, second], [first_old, second_old]) = credited_deposits(V83, true);
    let equity_before = h.equity_and_margin(second.account).0;
    let observed_at_seq = revision(&h, &first_old);
    let events = h
        .cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse { receipt: first_old.clone(), observed_at_seq })));
    assert_eq!(rejected(&events), None, "{events:?}");
    assert_eq!(h.balance(first.account), f("0"));
    let batch = moves(&h, [(&second_old, &moved(&second_old, 0, 0xc1), second.account)]);
    let events = reorganise(&mut h, batch);
    assert_eq!(rejected(&events), None, "{events:?}");
    assert_eq!(reversed(&events), vec![second_old.reference()]);
    assert_eq!(h.engine.accounts()[&second.account].positions.len(), 1);
    assert_eq!(h.equity_and_margin(second.account).0, equity_before);
}

#[test]
fn two_shifts_in_one_reorganisation_move_every_deposit_together() {
    // Four credited deposits at ids 0 to 3; the reorganisation includes one new deposit before id 0
    // and another before id 2, so the first two move up one id and the last two up two.
    let (mut h, clients, receipts) = reorg::four_credited_deposits();
    let (positions, cash) = (reorg::positions(&h, &clients), total_cash(&h));
    let shifted: Vec<_> = receipts
        .iter()
        .enumerate()
        .map(|(id, old)| moved(old, id as u64 + 1 + id as u64 / 2, 0xd0 + id as u8))
        .collect();
    let batch =
        moves(&h, receipts.iter().zip(&shifted).zip(&clients).map(|((old, new), client)| (old, new, client.account)));
    let events = reorganise(&mut h, batch);
    assert_eq!(rejected(&events), None, "{events:?}");
    assert!(!events.iter().any(|event| matches!(event, Event::CloseOut { .. })), "{events:?}");
    assert_eq!(reorg::positions(&h, &clients), positions);
    assert_eq!(total_cash(&h), cash);
    for (receipt, id) in shifted.iter().zip([1, 2, 4, 5]) {
        assert_eq!(receipt.deposit_id, id);
        assert_eq!(
            h.engine.recorded_deposit(&receipt.reference()).map(|r| r.status.clone()),
            Some(DepositStatus::Unfinalised)
        );
    }
}

#[test]
fn a_reorganisation_is_refused_whole_when_one_move_fails_its_checks() {
    let (mut h, [first, second], [first_old, second_old]) = credited_deposits(V83, true);
    let cash = total_cash(&h);
    let first_new = moved(&first_old, 1, 0xc0);
    let refusals: [(Vec<_>, DepositReject); 4] = [
        // The second client's receipt still holds id 1 and does not move.
        (moves(&h, [(&first_old, &first_new, first.account)]), DepositReject::ConflictingReceipt),
        // The second move names a revision the receipt has moved past.
        (
            {
                let mut batch = moves(
                    &h,
                    [
                        (&first_old, &first_new, first.account),
                        (&second_old, &moved(&second_old, 2, 0xc1), second.account),
                    ],
                );
                batch[1].observed_at_seq -= 1;
                batch
            },
            DepositReject::SettledReceipt,
        ),
        // Two moves of one deposit.
        (
            moves(
                &h,
                [(&first_old, &first_new, first.account), (&first_old, &moved(&first_old, 2, 0xc2), first.account)],
            ),
            DepositReject::InvalidReceipt,
        ),
        // Two replacements at one id.
        (
            moves(
                &h,
                [(&first_old, &first_new, first.account), (&second_old, &moved(&second_old, 1, 0xc1), second.account)],
            ),
            DepositReject::InvalidReceipt,
        ),
    ];
    for (batch, reason) in refusals {
        let events = reorganise(&mut h, batch);
        assert_eq!(rejected(&events), Some(reason), "{events:?}");
        assert!(!events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { .. })), "{events:?}");
        assert_eq!(total_cash(&h), cash);
    }
    assert_eq!(h.engine.accounts()[&second.account].positions.len(), 1);
    // Before `V83` the command does not exist.
    let (mut h, [first, _], [first_old, _]) = credited_deposits(ProtocolVersion::V80, true);
    let batch = moves(&h, [(&first_old, &moved(&first_old, 5, 0xc0), first.account)]);
    assert_eq!(rejected(&reorganise(&mut h, batch)), Some(DepositReject::ProtocolNotActive));
    let events = reorganise(&mut h, Vec::new());
    assert!(matches!(events[..], [Event::AdminRejected { .. }]), "an empty reorganisation is refused: {events:?}");
}
