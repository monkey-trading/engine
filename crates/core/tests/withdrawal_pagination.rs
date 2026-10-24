//! Bounded request-sequence pages and exact global FIFO ranks through `Engine::apply`.

mod common;
#[path = "withdrawals/setup.rs"]
mod setup;

use engine_core::command::Command;
use engine_core::withdrawal::{MAX_WITHDRAWAL_QUEUE_PAGE_SIZE, QueuedWithdrawal};
use engine_core::{Engine, Fixed};

use common::{Harness, acct, f};
use setup::{approvals, capital, deposit, withdraw};

#[test]
fn global_pages_seek_past_sequence_gaps_and_clamp_the_scan_bound() {
    let (h, seqs) = backlog(205);
    let page = h.engine.withdrawal_queue_page(None, None, 3);
    assert_eq!(positions(&page.queue), [(1, seqs[0]), (2, seqs[1]), (3, seqs[2])]);
    assert_eq!(page.next, Some(seqs[2]));

    let page = h.engine.withdrawal_queue_page(None, Some(seqs[199]), 3);
    assert_eq!(positions(&page.queue), [(201, seqs[200]), (202, seqs[201]), (203, seqs[202])]);
    assert_eq!(page.next, Some(seqs[202]));
    let page = h.engine.withdrawal_queue_page(None, Some(seqs[2] + 1), 1);
    assert_eq!(positions(&page.queue), [(4, seqs[3])]);
    assert_eq!(page.next, Some(seqs[3]));

    let page = h.engine.withdrawal_queue_page(None, None, usize::MAX);
    assert_eq!(page.queue.len(), MAX_WITHDRAWAL_QUEUE_PAGE_SIZE);
    assert_eq!(page.next, Some(seqs[MAX_WITHDRAWAL_QUEUE_PAGE_SIZE - 1]));
    let page = h.engine.withdrawal_queue_page(None, None, 0);
    assert_eq!(positions(&page.queue), [(1, seqs[0])]);
    assert_eq!(page.next, Some(seqs[0]));
    let page = h.engine.withdrawal_queue_page(None, Some(seqs[203]), 3);
    assert_eq!(positions(&page.queue), [(205, seqs[204])]);
    assert_eq!(page.next, None);
    let page = h.engine.withdrawal_queue_page(None, Some(u64::MAX), 3);
    assert!(page.queue.is_empty());
    assert_eq!(page.next, None);
    assert_eq!(h.engine.withdrawal_queue_position(seqs[204]), Some(205));
    assert_eq!(h.engine.withdrawal_queue_position(seqs[204] + 1), None);
}

#[test]
fn account_pages_advance_across_nonmatching_requests_with_exact_global_positions() {
    let (h, seqs) = backlog(9);
    let first = h.engine.withdrawal_queue_page(Some(acct(2)), None, 1);
    assert!(first.queue.is_empty(), "one scanned request cannot skip ahead to the matching second request");
    assert_eq!(first.next, Some(seqs[0]));
    let second = h.engine.withdrawal_queue_page(Some(acct(2)), first.next, 1);
    assert_eq!(positions(&second.queue), [(2, seqs[1])]);
    assert_eq!(second.next, Some(seqs[1]));

    for account in [acct(2), acct(99)] {
        let mut after = None;
        let mut found = Vec::new();
        for (index, seq) in seqs.iter().enumerate() {
            let page = h.engine.withdrawal_queue_page(Some(account), after, 1);
            assert!(page.queue.iter().all(|(_, request)| request.account == account));
            found.extend(positions(&page.queue));
            assert_eq!(page.next, (index + 1 < seqs.len()).then_some(*seq));
            after = page.next;
        }
        if account == acct(2) {
            assert_eq!(found, [(2, seqs[1]), (4, seqs[3]), (6, seqs[5]), (8, seqs[7])]);
        } else {
            assert!(found.is_empty());
        }
    }
}

#[test]
fn partial_heads_keep_rank_and_completion_or_empty_queue_reenqueue_preserves_ticket_order() {
    let mut h = setup::two_waiting();
    let before = h.engine.withdrawal_queue().copied().collect::<Vec<_>>();
    let (a, b) = (before[0], before[1]);
    assert_eq!((a.ticket, b.ticket), (0, 1));
    assert_eq!(h.engine.withdrawal_queue_position(a.requested_seq), Some(1));
    assert_eq!(h.engine.withdrawal_queue_position(b.requested_seq), Some(2));
    deposit(&mut h, acct(3), "40");
    assert_eq!(h.engine.withdrawal_queue().next().unwrap().remaining, f("40"));
    assert_eq!(h.engine.withdrawal_queue_position(a.requested_seq), Some(1));
    assert_eq!(h.engine.withdrawal_queue_position(b.requested_seq), Some(2));

    capital(&mut h, "60");
    assert_eq!(h.engine.withdrawal_queue_position(a.requested_seq), None);
    assert_eq!(h.engine.withdrawal_queue_position(b.requested_seq), Some(1));
    let page = h.engine.withdrawal_queue_page(None, Some(a.requested_seq), 1);
    assert_eq!(positions(&page.queue), [(1, b.requested_seq)]);
    assert_eq!((page.queue[0].1.ticket, page.queue[0].1.remaining), (b.ticket, f("90")));
    deposit(&mut h, acct(4), "90");
    let page = h.engine.withdrawal_queue_page(None, None, 1);
    assert!(page.queue.is_empty());
    assert_eq!(page.next, None);
    assert_eq!(h.engine.withdrawal_queue_position(b.requested_seq), None);

    assert!(approvals(&withdraw(&mut h, acct(3), "1")).is_empty());
    let next = h.engine.withdrawal_queue().next().unwrap();
    assert_eq!(next.ticket, b.ticket + 1);
    assert_eq!(h.engine.withdrawal_queue_position(next.requested_seq), Some(1));
    let page = h.engine.withdrawal_queue_page(None, Some(b.requested_seq), 1);
    assert_eq!(positions(&page.queue), [(1, next.requested_seq)]);
    assert_eq!(page.next, None);
}

#[test]
fn serialized_restart_and_replay_restore_positions_before_any_query_side_rebuild() {
    let (mut h, seqs) = backlog(7);
    capital(&mut h, "0.000003");
    let root = h.engine.state_root();
    let mut recovered: Engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
    let mut replay = Engine::new();
    for command in &h.log {
        replay.apply(command).unwrap();
    }
    for engine in [&h.engine, &recovered, &replay] {
        let page = engine.withdrawal_queue_page(None, None, 200);
        assert_eq!(positions(&page.queue), [(1, seqs[3]), (2, seqs[4]), (3, seqs[5]), (4, seqs[6])]);
        assert_eq!(page.next, None);
        assert_eq!(engine.withdrawal_queue_position(seqs[3]), Some(1));
        let page = engine.withdrawal_queue_page(Some(acct(2)), None, 2);
        assert_eq!(positions(&page.queue), [(1, seqs[3])]);
        assert_eq!(page.next, Some(seqs[4]));
    }
    assert_eq!(recovered.state_root(), root);
    assert_eq!(replay.state_root(), root);

    let events = withdraw(&mut h, acct(1), "0.000001");
    let command = h.log.last().unwrap();
    assert_eq!(recovered.apply(command).unwrap(), events);
    assert_eq!(replay.apply(command).unwrap(), events);
    let root = h.engine.state_root();
    assert_eq!(recovered.state_root(), root);
    assert_eq!(replay.state_root(), root);
    for engine in [&h.engine, &recovered, &replay] {
        assert_eq!(engine.withdrawal_queue().next_back().unwrap().ticket, 8);
        assert_eq!(engine.withdrawal_queue_position(command.seq), Some(5));
    }
}

/// Creates micro-USDC reservations from two accounts, inserting ticks so seq gaps cannot stand in
/// for queue positions. An already completed initial request also leaves the first live ticket > 0.
fn backlog(count: usize) -> (Harness, Vec<u64>) {
    let mut h = setup::v22();
    deposit(&mut h, acct(1), "10");
    deposit(&mut h, acct(2), "10");
    setup::win(&mut h, acct(1));
    assert_eq!(approvals(&withdraw(&mut h, acct(1), "30")), [(acct(1), f("30"), 1)]);
    assert_eq!(h.engine.withdrawal_cash(), Some(Fixed::ZERO));
    let mut seqs = Vec::new();
    for index in 0..count {
        seqs.push(h.log.len() as u64);
        assert!(approvals(&withdraw(&mut h, acct(if index % 2 == 0 { 1 } else { 2 }), "0.000001")).is_empty());
        h.cmd(Command::Tick);
    }
    (h, seqs)
}

/// Copies only the ranks and request sequences relevant to pagination assertions.
fn positions(queue: &[(u64, &QueuedWithdrawal)]) -> Vec<(u64, u64)> {
    queue.iter().map(|(position, request)| (*position, request.requested_seq)).collect()
}
