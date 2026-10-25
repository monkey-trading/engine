//! A full checkpoint posting queue defers checkpoints without stopping the sequencer.

use super::*;

#[test]
fn a_full_posting_queue_defers_checkpoints_and_the_next_one_covers_every_approval() {
    let mut node = Node::new("checkpoint-backlog");
    node.seq.sequence(deposit(1_000, "backlog-deposit")).unwrap().unwrap();
    // Every approval asks for a checkpoint, and this worker never takes one from its queue.
    let approvals = 80;
    for _ in 0..approvals {
        let events = node.seq.sequence(withdraw(1)).unwrap().expect("trusted time admits the request");
        assert!(events.iter().any(|event| matches!(event, Event::WithdrawalApproved { .. })), "{events:?}");
    }
    assert_eq!(node.work.capacity(), 0, "the posting queue filled");
    let gate = node.seq.gate.as_ref().unwrap();
    assert!(gate.backlogged);
    assert!(!gate.unproved.is_empty(), "approvals after the queue filled wait for a checkpoint");
    let last_checkpoint = log_entries(&node.dir)
        .into_iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(cp) => Some(cp.seq),
            _ => None,
        })
        .unwrap();
    assert!(last_checkpoint < node.seq.last_seq().unwrap(), "no checkpoint is written while the queue is full");

    // Once the worker takes one, the next approval's checkpoint covers every waiting approval.
    node._queued.try_recv().unwrap();
    node.seq.sequence(withdraw(1)).unwrap().unwrap();
    let gate = node.seq.gate.as_ref().unwrap();
    assert!(!gate.backlogged);
    assert!(gate.unproved.is_empty());
    assert!(gate.payouts.values().all(|payout| payout.proof.is_some()));
    assert_eq!(node.seq.engine.withdrawal_approvals().count(), approvals + 1);
}
