//! From `V83`, a write-off a deposit's reversal charged the house is repaid from the same deposit's next
//! credit: a client that lost part of its deposit to the house, had the deposit reversed above the
//! finalised frontier and written off below zero, and then had the same deposit included and credited
//! again, keeps what it would have had without the reorganisation, not its loss as well.

mod common;
#[allow(dead_code)]
#[path = "deposit_reorganisation/support.rs"]
mod reorg;
#[allow(dead_code)]
#[path = "staking/support.rs"]
mod support;

use alloy_primitives::B256;
use common::close_spread::{attest, evidence, trade};
use common::events::fills;
use common::{BTC, Client, Harness, f};
use engine_core::Fixed;
use engine_core::access::{AccessEvidence, SourceFailure, receipt_access_deposit};
use engine_core::command::Command;
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate, ReceiptMove};
use engine_core::eip712::deposit_screening_hash;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};
use reorg::{V83, credit, credited_deposits, moved, moves, receipt, rejected, reorganise, replace, revision};

/// The second of `credited_deposits`' clients, flat after closing its long at a loss, its deposit
/// reversed at the head: the write-off the reversal charged the house, and its cash and the house's
/// before the reversal.
fn reversed_at_a_loss(version: ProtocolVersion) -> (Harness, Client, DepositReceipt, Fixed, (Fixed, Fixed)) {
    let (mut h, [_, second], [_, second_old]) = credited_deposits(version, true);
    let sold = trade(&mut h, &second, BTC, Side::Sell, "0.05", "97000");
    assert_eq!(fills(&sold).len(), 1, "{sold:?}");
    let before = (h.balance(second.account), h.engine.house_cash());
    let observed_at_seq = revision(&h, &second_old);
    let reverse = DepositUpdate::Reverse { receipt: second_old.clone(), observed_at_seq };
    let events = h.cmd(Command::CustodyDeposit(Box::new(reverse)));
    assert_eq!(rejected(&events), None, "{events:?}");
    let written_off = events
        .iter()
        .find_map(|event| match event {
            Event::BadDebt { account, amount } if *account == second.account => Some(*amount),
            _ => None,
        })
        .expect("the reversal writes the loss off");
    assert_eq!((h.balance(second.account), h.engine.house_cash()), (f("0"), before.1 - written_off));
    (h, second, second_old, written_off, before)
}

/// What repaid the house among `events`.
fn repaid(events: &[Event]) -> Option<Fixed> {
    events.iter().find_map(|event| match event {
        Event::WriteOffRepaid { amount, .. } => Some(*amount),
        _ => None,
    })
}

#[test]
fn a_deposit_reversed_at_the_head_and_included_again_repays_the_write_off() {
    // Before `V83` the write-off stands, and the client keeps the loss the house wrote off.
    for (version, repays) in [(ProtocolVersion::V80, false), (V83, true)] {
        let (mut h, second, second_old, written_off, (cash, house)) = reversed_at_a_loss(version);
        // The sequencer includes the same transaction again a few blocks on, past the scan's head.
        let second_new = DepositReceipt { block: 9_020, block_hash: B256::repeat_byte(0xc5), ..second_old.clone() };
        let events = credit(&mut h, &second_new, second.account);
        assert_eq!(rejected(&events), None, "{events:?}");
        assert_eq!(repaid(&events), repays.then_some(written_off), "{version:?}: {events:?}");
        if repays {
            assert_eq!((h.balance(second.account), h.engine.house_cash()), (cash, house));
        } else {
            assert_eq!(h.balance(second.account), f("1000"));
        }
    }
}

#[test]
fn every_credit_that_returns_the_deposit_repays_the_write_off_once() {
    for path in ["credit", "reinstate", "retry", "replace", "reorganise"] {
        let (mut h, second, second_old, written_off, (cash, house)) = reversed_at_a_loss(V83);
        let events = match path {
            // The deposit returns in another block while the source is down, waits uncredited, and moves
            // again, screened clear, to a third block: alone, or as a reorganisation's move.
            "replace" | "reorganise" => {
                let pending = waiting(&mut h, &second, &second_old);
                let moved = DepositReceipt { block: 9_030, block_hash: B256::repeat_byte(0xc6), ..second_old.clone() };
                let seen = revision(&h, &pending);
                if path == "replace" {
                    replace(&mut h, &pending, seen, &moved, second.account)
                } else {
                    {
                        let batch = moves(&h, [(&pending, &moved, second.account)]);
                        reorganise(&mut h, batch)
                    }
                }
            }
            // The exact receipt is canonical again.
            "reinstate" => {
                let reversed_at_seq = revision(&h, &second_old);
                let reinstate = DepositUpdate::Reinstate { receipt: second_old.clone(), reversed_at_seq };
                h.cmd(Command::CustodyDeposit(Box::new(reinstate)))
            }
            // The deposit returns in another block while the screening source is down, so it waits
            // uncredited, and a retry with clear evidence credits it.
            "retry" => {
                let pending = waiting(&mut h, &second, &second_old);
                retry(&mut h, &pending, second.account)
            }
            _ => {
                let second_new =
                    DepositReceipt { block: 9_020, block_hash: B256::repeat_byte(0xc5), ..second_old.clone() };
                credit(&mut h, &second_new, second.account)
            }
        };
        assert_eq!(rejected(&events), None, "{path}: {events:?}");
        assert_eq!(repaid(&events), Some(written_off), "{path}: {events:?}");
        assert_eq!((h.balance(second.account), h.engine.house_cash()), (cash, house), "{path}");
        // Repaid once: the deposit's later finality moves no cash.
        let live = h
            .engine
            .unfinalised_deposit_records(0, u64::MAX)
            .into_iter()
            .find(|recorded| recorded.receipt.tx_hash == second_old.tx_hash)
            .expect("the deposit is live");
        let finalise = DepositUpdate::Finalise { receipt: live.receipt, observed_at_seq: live.updated_at_seq };
        let events = h.cmd(Command::CustodyDeposit(Box::new(finalise)));
        assert_eq!((repaid(&events), h.balance(second.account)), (None, cash), "{path}");
    }
}

#[test]
fn a_reversal_that_writes_nothing_off_is_owed_nothing() {
    // A flat client whose cash covers the reversal is left with the rest, and the deposit's return
    // credits it whole.
    let (mut h, [_, second], [_, second_old]) = credited_deposits(V83, false);
    common::close_spread::fund(&mut h, &second, "500");
    let observed_at_seq = revision(&h, &second_old);
    let reverse = DepositUpdate::Reverse { receipt: second_old.clone(), observed_at_seq };
    let events = h.cmd(Command::CustodyDeposit(Box::new(reverse)));
    assert!(!events.iter().any(|event| matches!(event, Event::BadDebt { .. })), "{events:?}");
    assert_eq!(h.balance(second.account), f("500"));
    let second_new = DepositReceipt { block: 9_020, block_hash: B256::repeat_byte(0xc5), ..second_old.clone() };
    let events = credit(&mut h, &second_new, second.account);
    assert_eq!(repaid(&events), None, "{events:?}");
    assert_eq!(h.balance(second.account), f("1500"));
}

/// The deposit `old` included again in another block while the screening source is down: it waits
/// uncredited and repays nothing.
fn waiting(h: &mut Harness, client: &Client, old: &DepositReceipt) -> DepositReceipt {
    let again = DepositReceipt { block: 9_020, block_hash: B256::repeat_byte(0xc5), ..old.clone() };
    let unavailable = AccessEvidence::Failed { reason: SourceFailure::Unavailable };
    let command =
        Command::AccessCustodyDeposit { receipt: again.clone(), payer: Some(client.account), evidence: unavailable };
    let events = h.cmd(command);
    assert!(events.iter().any(|event| matches!(event, Event::DepositPending { .. })), "{events:?}");
    assert_eq!(repaid(&events), None, "an uncredited receipt repays nothing");
    again
}

/// A screening retry of the waiting `receipt`, paid by `payer`, with clear evidence.
fn retry(h: &mut Harness, receipt: &DepositReceipt, payer: AccountId) -> Vec<Event> {
    let deposit = receipt_access_deposit(receipt, Some(payer));
    let ev = evidence(h, deposit_screening_hash(&deposit), deposit.account, deposit.payer);
    h.cmd(Command::RetryDepositScreening { chain_ref: receipt.reference(), evidence: ev })
}

#[test]
fn a_credit_of_the_same_transaction_for_another_account_repays_nothing() {
    // The reorganisation runs the transaction again and it now deposits for the first client: that
    // credit repays nothing of the second client's write-off, which its own deposit's return repays.
    let (mut h, second, second_old, written_off, (cash, _)) = reversed_at_a_loss(V83);
    let first = Client::new(81);
    let first_before = h.balance(first.account);
    let other = DepositReceipt {
        block: 9_020,
        block_hash: B256::repeat_byte(0xc5),
        beneficiary: DepositBeneficiary::Trader { account: first.account },
        ..second_old.clone()
    };
    let events = credit(&mut h, &other, first.account);
    assert_eq!((rejected(&events), repaid(&events)), (None, None), "{events:?}");
    assert_eq!(h.balance(first.account), first_before + f("1000"));
    // Another reorganisation drops that receipt, and the second client's own deposit returns.
    let observed_at_seq = revision(&h, &other);
    h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse { receipt: other, observed_at_seq })));
    let reversed_at_seq = revision(&h, &second_old);
    let events =
        h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reinstate { receipt: second_old, reversed_at_seq })));
    assert_eq!(repaid(&events), Some(written_off), "{events:?}");
    assert_eq!(h.balance(second.account), cash);
}

#[test]
fn a_write_off_after_an_untradeable_market_reopens_is_repaid_too() {
    // The second client's deposit reverses while BTC has no price, so its long cannot close out; the
    // write-off comes with the next price, and the deposit's return repays it.
    let (mut h, [_, second], [_, second_old]) = credited_deposits(V83, true);
    let house = h.engine.house_cash();
    h.advance(10 * 60 * 1000);
    h.cmd(Command::Tick);
    let observed_at_seq = revision(&h, &second_old);
    let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse {
        receipt: second_old.clone(),
        observed_at_seq,
    })));
    assert!(
        !events.iter().any(|event| matches!(event, Event::BadDebt { .. })),
        "no close-out without a price: {events:?}"
    );
    assert_eq!(h.engine.accounts()[&second.account].positions.len(), 1);
    let events = h.price(BTC, "97000");
    let written_off = events
        .iter()
        .find_map(|event| match event {
            Event::BadDebt { account, amount } if *account == second.account => Some(*amount),
            _ => None,
        })
        .expect("the price closes the client out and writes its shortfall off");
    let again = DepositReceipt { block: 9_020, block_hash: B256::repeat_byte(0xc5), ..second_old };
    let events = credit(&mut h, &again, second.account);
    assert_eq!(repaid(&events), Some(written_off), "{events:?}");
    assert_eq!(h.balance(second.account), f("1000") - written_off);
    assert!(h.engine.house_cash() >= house, "the house is whole again, its spreads kept");
}

#[test]
fn a_write_off_across_two_deposits_that_lose_their_credit_together_is_repaid_by_both() {
    // A client's two deposits of 1,000 USDC move in one reorganisation to a payer the screening source
    // cannot answer for, so both lose their credit at once, and the client's loss, more than either
    // deposit, is written off. Each deposit's credit on a later retry repays its part.
    let (mut h, _, _) = credited_deposits(V83, false);
    let client = Client::new(0x5c);
    attest(&mut h, &client);
    let deposits =
        [receipt(7, 9_100, 0xd1, 0xe1, client.account, "1000"), receipt(8, 9_101, 0xd2, 0xe2, client.account, "1000")];
    for deposit in &deposits {
        assert_eq!(rejected(&credit(&mut h, deposit, client.account)), None);
    }
    // A long closed 10% lower loses about 1,200 USDC of the 2,000.
    h.price(BTC, "100000");
    let opened = trade(&mut h, &client, BTC, Side::Buy, "0.12", "100000");
    assert_eq!(fills(&opened).len(), 1, "{opened:?}");
    h.advance(61_000);
    h.price(BTC, "90000");
    let closed = trade(&mut h, &client, BTC, Side::Sell, "0.12", "90000");
    assert_eq!(fills(&closed).len(), 1, "{closed:?}");
    let cash = h.balance(client.account);
    assert!(cash > f("0") && cash < f("1000"), "{cash}");
    let payer = AccountId([0x5a; 20]);
    let moved: Vec<DepositReceipt> = deposits.iter().map(|old| moved(old, old.deposit_id + 10, 0xd3)).collect();
    let unavailable = AccessEvidence::Failed { reason: SourceFailure::Unavailable };
    let batch = moves(&h, deposits.iter().zip(&moved).map(|(old, new)| (old, new, payer)))
        .into_iter()
        .map(|move_| ReceiptMove { evidence: Some(unavailable.clone()), ..move_ })
        .collect();
    let events = reorganise(&mut h, batch);
    let written_off: Vec<Fixed> = events
        .iter()
        .filter_map(|event| match event {
            Event::BadDebt { account, amount } if *account == client.account => Some(*amount),
            _ => None,
        })
        .collect();
    assert_eq!(written_off, [f("2000") - cash], "{events:?}");
    assert!(written_off[0] > f("1000"), "the write-off is more than either deposit");
    let mut repaid_total = Fixed::ZERO;
    for receipt in &moved {
        let events = retry(&mut h, receipt, payer);
        assert_eq!(rejected(&events), None, "{events:?}");
        repaid_total += repaid(&events).unwrap_or(Fixed::ZERO);
    }
    assert_eq!(repaid_total, written_off[0]);
    assert_eq!(h.balance(client.account), cash);
}

#[test]
fn a_variant_of_the_same_transaction_with_another_amount_keeps_the_first_deposits_repayment() {
    // The reorganisation runs the second client's transaction again for 500 USDC, credits that variant
    // and drops it in turn; the 1,000 USDC deposit then returns and repays its write-off.
    let (mut h, second, second_old, written_off, (cash, _)) = reversed_at_a_loss(V83);
    let variant =
        DepositReceipt { block: 9_020, block_hash: B256::repeat_byte(0xc5), amount: f("500"), ..second_old.clone() };
    let events = credit(&mut h, &variant, second.account);
    assert_eq!((rejected(&events), repaid(&events)), (None, None), "{events:?}");
    let observed_at_seq = revision(&h, &variant);
    let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse { receipt: variant, observed_at_seq })));
    assert!(!events.iter().any(|event| matches!(event, Event::BadDebt { .. })), "{events:?}");
    let reversed_at_seq = revision(&h, &second_old);
    let reinstate = DepositUpdate::Reinstate { receipt: second_old, reversed_at_seq };
    let events = h.cmd(Command::CustodyDeposit(Box::new(reinstate)));
    assert_eq!(repaid(&events), Some(written_off), "{events:?}");
    assert_eq!(h.balance(second.account), cash);
}

#[test]
fn a_reversed_deposit_record_is_committed_incrementally_as_a_rebuild_commits_it() {
    let (mut h, second, second_old, _, _) = reversed_at_a_loss(V83);
    for step in ["kept and counted", "repaid and released"] {
        let incremental = h.engine.state_root();
        let mut rebuilt: engine_core::Engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
        assert_eq!(rebuilt.state_root(), incremental, "{step}");
        let again = DepositReceipt { block: 9_020, block_hash: B256::repeat_byte(0xc5), ..second_old.clone() };
        if step == "kept and counted" {
            assert!(repaid(&credit(&mut h, &again, second.account)).is_some());
        }
    }
}

#[test]
fn a_deposit_reversed_after_a_write_off_repays_none_of_it() {
    // The second client's 1,000 USDC deposit is reversed at a loss and written off, and never returns.
    // The client then deposits 500, which a reorganisation drops while the client is flat, writing
    // nothing off, and includes again: its absence prevented no write-off, so it repays none.
    let (mut h, second, second_old, written_off, _) = reversed_at_a_loss(V83);
    let later = receipt(9, 9_200, 0xd5, 0xe5, second.account, "500");
    assert_eq!(rejected(&credit(&mut h, &later, second.account)), None);
    let observed_at_seq = revision(&h, &later);
    let events =
        h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse { receipt: later.clone(), observed_at_seq })));
    assert!(!events.iter().any(|event| matches!(event, Event::BadDebt { .. })), "{events:?}");
    let again = DepositReceipt { block: 9_210, block_hash: B256::repeat_byte(0xd6), ..later };
    let events = credit(&mut h, &again, second.account);
    assert_eq!(repaid(&events), None, "{events:?}");
    assert_eq!(h.balance(second.account), f("500"));
    // The first deposit's return still repays its own write-off.
    let reversed_at_seq = revision(&h, &second_old);
    let events =
        h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reinstate { receipt: second_old, reversed_at_seq })));
    assert_eq!(repaid(&events), Some(written_off), "{events:?}");
}

#[test]
fn a_write_off_while_two_deposits_are_missing_is_repaid_once_by_whichever_returns_first() {
    // A client's two deposits of 1,000 USDC are dropped one after the other, and the second drop leaves
    // it below zero. Either deposit's presence would have prevented the write-off, so whichever the chain
    // includes again first repays all of it, and the other then repays nothing.
    for later_first in [true, false] {
        let (mut h, _, _) = credited_deposits(V83, false);
        let client = Client::new(0x5d);
        attest(&mut h, &client);
        let deposits = [
            receipt(7, 9_100, 0xd1, 0xe1, client.account, "1000"),
            receipt(8, 9_101, 0xd2, 0xe2, client.account, "1000"),
        ];
        for deposit in &deposits {
            assert_eq!(rejected(&credit(&mut h, deposit, client.account)), None);
        }
        // A long closed 10% lower loses about 600 USDC of the 2,000.
        h.price(BTC, "100000");
        let opened = trade(&mut h, &client, BTC, Side::Buy, "0.06", "100000");
        assert_eq!(fills(&opened).len(), 1, "{opened:?}");
        h.advance(61_000);
        h.price(BTC, "90000");
        let closed = trade(&mut h, &client, BTC, Side::Sell, "0.06", "90000");
        assert_eq!(fills(&closed).len(), 1, "{closed:?}");
        let cash = h.balance(client.account);
        assert!(cash > f("1000") && cash < f("2000"), "{cash}");
        let mut written_off = Vec::new();
        for deposit in &deposits {
            let observed_at_seq = revision(&h, deposit);
            let reverse = DepositUpdate::Reverse { receipt: deposit.clone(), observed_at_seq };
            let events = h.cmd(Command::CustodyDeposit(Box::new(reverse)));
            written_off.extend(events.iter().filter_map(|event| match event {
                Event::BadDebt { account, amount } if *account == client.account => Some(*amount),
                _ => None,
            }));
        }
        assert_eq!(written_off, [f("2000") - cash], "only the second drop writes off");
        let order = if later_first { [1, 0] } else { [0, 1] };
        let repaid_each: Vec<Option<Fixed>> = order
            .into_iter()
            .map(|index| {
                let deposit = &deposits[index];
                let again = DepositReceipt {
                    block: deposit.block + 20,
                    block_hash: B256::repeat_byte(0xd6),
                    ..deposit.clone()
                };
                let events = credit(&mut h, &again, client.account);
                assert_eq!(rejected(&events), None, "{events:?}");
                repaid(&events)
            })
            .collect();
        assert_eq!(repaid_each, [Some(written_off[0]), None], "later first: {later_first}");
        assert_eq!(h.balance(client.account), cash);
    }
}
