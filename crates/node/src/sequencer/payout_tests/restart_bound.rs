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

#[test]
fn a_newer_approval_waits_for_an_older_one_that_needs_a_signature_in_an_unloaded_window() {
    // The older approval is unsigned, or holds only an authorization of the epoch custody retired.
    for rotated in [false, true] {
        let mut node = Node::new(if rotated { "payout-frontier-retired" } else { "payout-frontier-unsigned" });
        node.seq.sequence(deposit(10_000, "frontier-deposit")).unwrap().unwrap();
        let window = BACKLOG_WINDOW as u64;
        let older = window + 1;
        for _ in 0..older {
            node.seq.sequence(withdraw(1)).unwrap().unwrap();
            while node._queued.try_recv().is_ok() {}
        }
        // Before the restart the first window is signed by the latest epoch and `older` is not.
        let cp = node.last_checkpoint();
        let mut evidence = node.evidence(&cp);
        evidence.approvals =
            (1..=if rotated { older } else { window }).map(|nonce| status(&node, nonce, false)).collect();
        assert_eq!(
            nonces(release_all(&mut node, &evidence)),
            (1..=evidence.approvals.len() as u64).collect::<Vec<_>>()
        );
        if rotated {
            evidence.signer_epoch = 2;
            evidence.approvals.pop();
            assert_eq!(release_all(&mut node, &evidence).len(), BACKLOG_WINDOW);
        }
        let mut recovered = node.restart(&node.last_checkpoint());
        let gate = recovered.seq.gate.as_ref().unwrap();
        assert_eq!((gate.payouts.len(), gate.backlog.len()), (BACKLOG_WINDOW, 1), "`older` waits unloaded");

        // A live approval is observed unpaid while claims retire all of the first window but one.
        recovered.seq.sequence(withdraw(1)).unwrap().unwrap();
        while node._queued.try_recv().is_ok() {}
        let live = older + 1;
        evidence.checkpoint_seq = Some(node.last_checkpoint().seq);
        evidence.checkpoint_root = Some(node.last_checkpoint().root);
        evidence.approvals = (1..window).map(|nonce| status(&recovered, nonce, true)).collect();
        evidence.approvals.push(status(&recovered, live, false));
        assert!(release_all(&mut recovered, &evidence).is_empty(), "the live approval passed an unloaded one");

        // The last claim loads `older`, which custody has not reported yet.
        evidence.approvals = vec![status(&recovered, window, true), status(&recovered, live, false)];
        assert!(release_all(&mut recovered, &evidence).is_empty(), "the live approval passed an unobserved one");
        while node._queued.try_recv().is_ok() {}
        evidence.checkpoint_seq = Some(node.last_checkpoint().seq);
        evidence.checkpoint_root = Some(node.last_checkpoint().root);
        evidence.approvals = vec![status(&recovered, older, false), status(&recovered, live, false)];
        assert_eq!(nonces(release_all(&mut recovered, &evidence)), [older, live], "approval order; rotated={rotated}");
    }
}

/// Custody's report on `nonce`, one of the trader's approvals.
fn status(node: &Node, nonce: u64, claimed: bool) -> ApprovalStatus {
    let approved_seq = node.seq.engine.withdrawal_approval(TRADER, nonce).unwrap().approved_seq;
    ApprovalStatus { approval: ApprovalId { account: TRADER, nonce, approved_seq }, claimed }
}

/// Applies `evidence` until it signs nothing more, returning what it signed in order.
fn release_all(node: &mut Node, evidence: &LifecycleEvidence) -> Vec<SignedWithdrawal> {
    let mut signed = Vec::new();
    for _ in 0..16 {
        node.seq.release_payouts(evidence).unwrap();
        node.seq.flush().unwrap();
        let before = signed.len();
        while let Ok(withdrawal) = node.signed.try_recv() {
            assert_eq!(withdrawal.authorization.as_ref().unwrap().signer_epoch, evidence.signer_epoch);
            signed.push(withdrawal);
        }
        if signed.len() == before && node.seq.gate.as_ref().unwrap().schedule.next().is_none() {
            break;
        }
    }
    signed
}

fn nonces(signed: Vec<SignedWithdrawal>) -> Vec<u64> {
    signed.into_iter().map(|withdrawal| withdrawal.nonce).collect()
}
