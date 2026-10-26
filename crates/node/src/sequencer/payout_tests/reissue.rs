//! Reissues preserve real signature order across bounded custody pages.

use super::*;

use crate::checkpoints::APPROVAL_PAGE_SIZE;

#[test]
fn paginated_expiry_and_same_key_epoch_changes_preserve_approval_order() {
    for rotate in [false, true] {
        let mut node = Node::new(if rotate { "reissue-epoch-pages" } else { "reissue-expiry-pages" });
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
        node.seq.checkpoint_status(Some(cp.seq), crate::checkpoints::Health::Healthy).unwrap();
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
            node.clock.advance(Duration::from_secs(crate::withdrawals::LIFETIME_SECONDS - 1));
            node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
            let early = deliver_pages(&mut node, &evidence, APPROVAL_PAGE_SIZE, false);
            assert_eq!(early, expected[..APPROVAL_PAGE_SIZE]);
            node.clock.advance(Duration::from_secs(crate::withdrawals::LIFETIME_SECONDS));
            node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
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
