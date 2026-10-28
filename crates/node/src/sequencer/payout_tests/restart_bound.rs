//! Restart work follows the unpaid approvals, not every approval ever made.

use super::*;
use crate::sequencer::payouts::BACKLOG_WINDOW;

#[test]
fn restart_loads_approvals_in_fifo_windows_and_keeps_only_the_unpaid() {
    let mut node = Node::new("payout-restart-bound");
    node.seq.sequence(deposit(10_000, "bound-deposit")).unwrap().unwrap();
    let total = 2 * BACKLOG_WINDOW + 40;
    for _ in 0..total {
        node.seq.sequence(withdraw(1)).unwrap().unwrap();
        while node._queued.try_recv().is_ok() {}
    }
    let unpaid = [3, BACKLOG_WINDOW as u64 + 7, total as u64];
    let cp = node.last_checkpoint();
    let (key, domain) = (node.seq.key.clone(), node.domain());
    let cfg = SequencerConfig {
        checkpoint_parent: crate::log::CheckpointParent {
            has_checkpoint: true,
            seq: cp.seq,
            root: cp.root.parse().unwrap(),
        },
        ..node.config()
    };
    let log = Sequencer::read_log_for_recovery(
        &node.dir,
        Some(domain),
        Some(AccountId(key.address())),
        Some(cfg.checkpoint_parent),
        |_| {},
    )
    .unwrap();
    let mut recovered =
        Sequencer::recover_from(log, cfg, key, broadcast::channel(128).0, mpsc::unbounded_channel().0).unwrap();

    // Only the oldest window is loaded, proved and registered; the rest waits in FIFO order.
    let gate = recovered.gate.as_ref().unwrap();
    assert_eq!(gate.payouts.len(), BACKLOG_WINDOW);
    assert_eq!(gate.backlog.len(), total - BACKLOG_WINDOW);
    assert!(gate.unproved.is_empty() && gate.payouts.values().all(|payout| payout.proof.is_some()));
    assert_eq!(gate.approvals.as_ref().unwrap().lock().unwrap().len(), BACKLOG_WINDOW);
    assert!(gate.payouts.keys().all(|(_, nonce)| *nonce <= BACKLOG_WINDOW as u64), "the oldest approvals load first");

    // A new approval cannot be released ahead of the older ones that custody has not been asked about.
    recovered.trust(sample_around(node.clock.as_ref())).unwrap();
    recovered.checkpoint_status(Some(cp.seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    recovered.sequence(withdraw(1)).unwrap().unwrap();
    let latest = node.last_checkpoint();
    let mut evidence = node.evidence(&latest);
    let live = recovered.engine.withdrawal_approval(TRADER, total as u64 + 1).copied().unwrap();
    evidence.approvals = vec![ApprovalStatus {
        approval: ApprovalId { account: TRADER, nonce: live.nonce, approved_seq: live.approved_seq },
        claimed: false,
    }];
    let (mut sweeps, mut peak) = (0, 0);
    recovered.release_payouts(&evidence).unwrap();
    recovered.flush().unwrap();

    // Custody reports every approval paid but three; the sweep drops each paid one as it is observed.
    while !recovered.gate.as_ref().unwrap().backlog.is_empty() || recovered.gate.as_ref().unwrap().unswept > 0 {
        let gate = recovered.gate.as_ref().unwrap();
        peak = peak.max(gate.payouts.len());
        evidence.approvals = gate
            .payouts
            .values()
            .filter(|payout| payout.recovered)
            .map(|payout| ApprovalStatus {
                approval: ApprovalId {
                    account: payout.approval.account,
                    nonce: payout.approval.nonce,
                    approved_seq: payout.approval.approved_seq,
                },
                claimed: !unpaid.contains(&payout.approval.nonce),
            })
            .collect();
        recovered.release_payouts(&evidence).unwrap();
        sweeps += 1;
    }
    assert!(sweeps > 1 && peak <= BACKLOG_WINDOW + unpaid.len() + 1, "peak {peak} follows the window, not {total}");
    let gate = recovered.gate.as_ref().unwrap();
    // The unpaid approvals and the live one stay, each proved; nothing else does.
    assert_eq!(gate.payouts.len(), unpaid.len() + 1);
    assert!(gate.payouts.values().all(|payout| payout.proof.is_some()));
    assert_eq!(gate.approvals.as_ref().unwrap().lock().unwrap().len(), unpaid.len() + 1);
}
