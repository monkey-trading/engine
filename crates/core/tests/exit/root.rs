//! The exit terms' independence from how the checker's roots are taken: the committed terms are a
//! function of the state alone, so a checker that computes the root at every checkpoint and one
//! that computes it only at the last command must see the same leaf.

use alloy_primitives::B256;
use engine_core::Fixed;
use engine_core::access::SanctionsStatus;
use engine_core::command::Command;
use engine_core::deposits::{CustodyCheckpoint, DepositUpdate};
use engine_core::event::Event;
use engine_core::merkle::ExitTerms;

use crate::access_support::attest;
use crate::common::{Client, f};
use crate::custody::{assert_root_matches_rebuild, credit, custody, receipt, retry};

#[test]
fn the_exit_terms_do_not_depend_on_how_often_the_root_is_taken() {
    // Settle the fixture's deposits through a finalised frontier, credit one deposit past it, and
    // reverse that one: its principal joins the reservation the terms exclude past the highest live
    // id. A wider finalised frontier then settles the reversed id below the cursor, and pruning it
    // must return the principal from that reservation: a checker that takes the root at every
    // checkpoint and one that takes it only at the last command must see the same terms.
    fn run(dense: bool) -> (B256, ExitTerms) {
        let (mut h, [client, newcomer, _], receipts) = custody();
        attest(&mut h, &Client::new(2));
        for receipt in [&receipts[1], &receipts[3]] {
            let events = retry(&mut h, receipt, SanctionsStatus::Clear);
            assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
        }
        let mut frontier =
            CustodyCheckpoint { block: receipts[0].block, block_hash: receipts[0].block_hash, deposit_count: 1 };
        for receipt in &receipts[..] {
            let seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
            let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Finalise {
                receipt: receipt.clone(),
                observed_at_seq: seq,
            })));
            assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
        }
        let events = h.cmd(Command::CustodyFinalised(frontier));
        assert!(!events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{events:?}");
        if dense {
            h.engine.state_root();
        }
        let extra = receipt(4, Some(client), "10");
        let events = credit(&mut h, &extra, Some(client), SanctionsStatus::Clear);
        assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
        if dense {
            h.engine.state_root();
        }
        let seq = h.engine.recorded_deposit(&extra.reference()).unwrap().updated_at_seq;
        let reverse = DepositUpdate::Reverse { receipt: extra.clone(), observed_at_seq: seq };
        let events = h.cmd(Command::CustodyDeposit(Box::new(reverse)));
        assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
        if dense {
            h.engine.state_root();
        }
        frontier = CustodyCheckpoint { block: extra.block, block_hash: extra.block_hash, deposit_count: 5 };
        let events = h.cmd(Command::CustodyFinalised(frontier));
        assert!(!events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{events:?}");
        let root = B256::from(h.engine.state_root());
        let terms = h.engine.exit_proof(client).unwrap().terms;
        assert_eq!(
            (terms.terms.deposit_cursor, &terms.terms.gaps, terms.terms.unheld, terms.terms.held_receipts),
            (5, &Vec::<u64>::new(), Fixed::ZERO, Fixed::ZERO)
        );
        assert_eq!(h.engine.accounts()[&newcomer].balance, f("40"));
        assert!(terms.verify(root));
        assert_root_matches_rebuild(&mut h.engine);
        (root, terms.terms)
    }
    assert_eq!(run(true), run(false));
}
