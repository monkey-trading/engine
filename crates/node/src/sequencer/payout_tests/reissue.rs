//! Rotation and holder requests reissue in real signature order across bounded custody pages.

use super::*;

use crate::checkpoints::APPROVAL_PAGE_SIZE;

#[test]
fn paginated_requests_and_same_key_epoch_changes_preserve_approval_order() {
    for rotate in [false, true] {
        let mut node = Node::new(if rotate { "reissue-epoch-pages" } else { "reissue-request-pages" });
        let count = APPROVAL_PAGE_SIZE + 3;
        for index in 0..count {
            let mut address = [0; 20];
            address[12..].copy_from_slice(&(1_000_u64 - index as u64).to_be_bytes());
            let account = AccountId(address);
            node.seq
                .sequence(Command::Deposit {
                    account,
                    amount: Fixed::from_int(20),
                    chain_ref: format!("reissue-fixture-{index}"),
                })
                .unwrap()
                .unwrap();
            node.seq.sequence(Command::Withdraw { account, amount: Fixed::ONE, auth: None }).unwrap().unwrap();
            while node._queued.try_recv().is_ok() {}
        }
        let cp = log_entries(&node.dir)
            .into_iter()
            .rev()
            .find_map(|entry| match entry {
                LogEntry::Checkpoint(cp) => Some(cp),
                _ => None,
            })
            .unwrap();
        node.seq.checkpoint_status(Some(cp.seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
        let gate = node.seq.gate.as_ref().unwrap();
        let mut ordered: Vec<_> = gate.payouts.values().map(|payout| (payout.order, payout.approval)).collect();
        ordered.sort_by_key(|(order, _)| *order);
        let expected: Vec<_> = ordered.iter().map(|(_, approval)| (approval.account, approval.nonce)).collect();
        let balances = node.seq.engine.accounts().clone();
        let mut evidence = node.evidence(&cp);
        evidence.approvals = ordered
            .iter()
            .map(|(_, approval)| ApprovalStatus {
                approval: ApprovalId {
                    account: approval.account,
                    nonce: approval.nonce,
                    approved_seq: approval.approved_seq,
                },
                claimed: false,
            })
            .collect();
        evidence.approvals.sort_by_key(|status| (status.approval.account, status.approval.nonce));
        assert_ne!(evidence.approvals[0].approval.account, expected[0].0);
        let initial = deliver_pages(&mut node, &evidence, count, !rotate);
        assert_eq!(initial, expected, "initial authorization order");
        if rotate {
            evidence.signer_epoch += 1;
        } else {
            node.clock.advance(Duration::from_secs(crate::withdrawals::LIFETIME_SECONDS));
            node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
            assert!(deliver_pages_silently(&mut node, &evidence), "expiry alone signs nothing");
            for key in expected.iter().rev() {
                let replaces = node.seq.gate.as_ref().unwrap().payouts[key].released.unwrap().1;
                assert_eq!(node.seq.request_reissue(key.0, key.1, replaces), Reissue::Queued);
            }
        }
        let reissued = deliver_pages(&mut node, &evidence, count, false);
        assert_eq!(reissued, expected, "replacement authorization order; rotate={rotate}");
        assert_eq!(node.seq.engine.accounts(), &balances, "reissuing never repeats a ledger debit");
        assert_eq!(node.seq.engine.withdrawal_approvals().count(), count);
    }
}

/// Drives actual signing with bounded account-sorted pages, whose order opposes FIFO.
fn deliver_pages(
    node: &mut Node,
    evidence: &LifecycleEvidence,
    count: usize,
    mut stagger: bool,
) -> Vec<(AccountId, u64)> {
    let mut delivered = Vec::new();
    for _ in 0..6 {
        for page in evidence.approvals.chunks(APPROVAL_PAGE_SIZE) {
            let mut observed = evidence.clone();
            observed.approvals = page.to_vec();
            node.seq.release_payouts(&observed).unwrap();
            node.seq.flush().unwrap();
            while let Ok(withdrawal) = node.signed.try_recv() {
                let auth = withdrawal.authorization.as_ref().unwrap();
                assert_eq!(auth.signer_epoch, evidence.signer_epoch);
                crate::withdrawals::verify(&withdrawal, node.domain(), evidence.signer).unwrap();
                delivered.push((withdrawal.account, withdrawal.nonce));
            }
            if stagger && delivered.len() == APPROVAL_PAGE_SIZE {
                node.clock.advance(Duration::from_secs(1));
                node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
                stagger = false;
            }
        }
        if delivered.len() == count {
            break;
        }
    }
    assert_eq!(delivered.len(), count, "every unpaid approval eventually receives one usable authorization");
    delivered
}

/// Whether every page of custody evidence leaves the log and stream unchanged.
fn deliver_pages_silently(node: &mut Node, evidence: &LifecycleEvidence) -> bool {
    for _ in 0..3 {
        for page in evidence.approvals.chunks(APPROVAL_PAGE_SIZE) {
            let mut observed = evidence.clone();
            observed.approvals = page.to_vec();
            node.seq.release_payouts(&observed).unwrap();
            node.seq.flush().unwrap();
            if node.signed.try_recv().is_ok() {
                return false;
            }
        }
    }
    true
}

#[test]
fn a_request_reissues_only_an_expired_authorization_and_only_once() {
    let mut node = Node::new("reissue-on-request");
    let cp = node.approve();
    let lifetime = Duration::from_secs(crate::withdrawals::LIFETIME_SECONDS);
    assert_eq!(node.seq.request_reissue(TRADER, 1, 0), Reissue::Queued, "the first authorization is still unsigned");
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    let first = node.signed.try_recv().unwrap();
    let expires_at = first.authorization.as_ref().unwrap().expires_at;
    assert_eq!(node.seq.request_reissue(TRADER, 1, expires_at), Reissue::StillValid { expires_at });
    node.clock.advance(lifetime - Duration::from_secs(1));
    node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
    let valid = node.seq.request_reissue(TRADER, 1, expires_at);
    assert_eq!(valid, Reissue::StillValid { expires_at }, "one second before expiry");
    assert_eq!(node.seq.request_reissue(TRADER, 2, 0), Reissue::Unknown, "no such approval");
    node.clock.advance(Duration::from_secs(1));
    node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    assert!(node.signed.try_recv().is_err(), "expiry alone signs nothing");

    let stale = node.seq.request_reissue(TRADER, 1, expires_at - 1);
    assert_eq!(stale, Reissue::Stale { current: expires_at }, "a request names the authorization it replaces");
    assert_eq!(node.seq.request_reissue(TRADER, 1, expires_at), Reissue::Queued);
    let repeat = node.seq.request_reissue(TRADER, 1, expires_at);
    assert_eq!(repeat, Reissue::Queued, "a repeat before signing is absorbed");
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    let second = node.signed.try_recv().unwrap();
    assert_eq!((second.account, second.amount, second.nonce), (first.account, first.amount, first.nonce));
    let renewed = second.authorization.as_ref().unwrap();
    assert_eq!(renewed.issued_at, expires_at);
    assert_ne!(second.signature, first.signature);
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    assert!(node.signed.try_recv().is_err(), "one request yields one authorization");
    assert_eq!(
        node.seq.request_reissue(TRADER, 1, expires_at),
        Reissue::Stale { current: renewed.expires_at },
        "the same signed request cannot be replayed"
    );

    let mut paid = node.evidence(&cp);
    paid.approvals[0].claimed = true;
    node.seq.release_payouts(&paid).unwrap();
    assert_eq!(node.seq.request_reissue(TRADER, 1, renewed.expires_at), Reissue::Unknown, "claimed means done");
}

#[test]
fn an_authorization_of_a_retired_epoch_is_not_reported_usable() {
    let mut node = Node::new("reissue-retired-epoch");
    let cp = node.approve();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    let expires_at = node.signed.try_recv().unwrap().authorization.unwrap().expires_at;
    // Custody reports epoch 2 before the bounded scan has signed this approval again.
    let gate = node.seq.gate.as_mut().unwrap();
    gate.schedule.prepare(2, &gate.payouts);
    assert_eq!(node.seq.request_reissue(TRADER, 1, expires_at), Reissue::Queued);
}

#[test]
fn a_restart_keeps_a_signed_approval_idle_until_rotation_or_a_request() {
    let mut node = Node::new("reissue-restart");
    let cp = node.approve();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    let first = node.signed.try_recv().unwrap();
    let expires_at = first.authorization.as_ref().unwrap().expires_at;
    // Long after it expired, a same-key restart must not sign it again.
    node.clock.advance(Duration::from_secs(5 * crate::withdrawals::LIFETIME_SECONDS));
    let mut recovered = node.restart(&cp);
    recovered.seq.release_payouts(&recovered.evidence(&cp)).unwrap();
    recovered.seq.flush().unwrap();
    assert!(recovered.signed.try_recv().is_err(), "a restart is not a reissue trigger");
    let released = recovered.seq.gate.as_ref().unwrap().payouts[&(TRADER, 1)].released;
    assert_eq!(released, Some((1, expires_at)), "recovery restores the logged authorization");
    assert_eq!(recovered.seq.request_reissue(TRADER, 1, expires_at), Reissue::Queued);
    recovered.seq.release_payouts(&recovered.evidence(&cp)).unwrap();
    recovered.seq.flush().unwrap();
    assert_eq!(recovered.signed.try_recv().unwrap().nonce, 1, "a request still reissues after a restart");

    let mut rotated = recovered.evidence(&cp);
    rotated.signer_epoch = 2;
    recovered.seq.release_payouts(&rotated).unwrap();
    recovered.seq.flush().unwrap();
    let epoch = recovered.signed.try_recv().unwrap().authorization.unwrap().signer_epoch;
    assert_eq!(epoch, 2, "a rotation after a restart still reissues");
}
