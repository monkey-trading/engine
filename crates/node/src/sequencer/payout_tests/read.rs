//! The account payouts read answers from the gate's memory: signing progress, reissues, paid
//! approvals, and a restart that has not yet loaded every approval.

use super::*;
use crate::sequencer::payouts::BACKLOG_WINDOW;

const ALL: usize = usize::MAX;

fn status(node: &Node, nonce: u64, claimed: bool) -> ApprovalStatus {
    let approved_seq = node.seq.engine.withdrawal_approval(TRADER, nonce).unwrap().approved_seq;
    ApprovalStatus { approval: ApprovalId { account: TRADER, nonce, approved_seq }, claimed }
}

fn sign(node: &mut Node, evidence: &LifecycleEvidence) -> SignedWithdrawal {
    node.seq.release_payouts(evidence).unwrap();
    node.seq.flush().unwrap();
    node.signed.try_recv().expect("an authorization was signed")
}

#[test]
fn an_unsigned_then_signed_then_reissued_approval_reads_its_latest_authorization() {
    let mut node = Node::new("payouts-read-lifecycle");
    let cp = node.approve();

    let unsigned = node.seq.account_payouts(TRADER, None, ALL);
    assert_eq!(unsigned.seq, node.seq.engine.last_seq());
    assert!(unsigned.loaded && unsigned.next.is_none());
    assert_eq!(unsigned.payouts.len(), 1);
    let view = &unsigned.payouts[0];
    assert_eq!((view.nonce, view.amount, &view.withdrawal), (1, Fixed::from_int(40), &None), "not yet signed");

    let evidence = node.evidence(&cp);
    let first = sign(&mut node, &evidence);
    let signed = node.seq.account_payouts(TRADER, None, ALL);
    assert_eq!(signed.payouts[0].withdrawal.as_ref(), Some(&first), "the shape the stream carried");

    // A signer rotation replaces it.
    let mut rotated = node.evidence(&cp);
    rotated.signer_epoch = 2;
    let second = sign(&mut node, &rotated);
    assert_eq!(second.authorization.as_ref().unwrap().signer_epoch, 2);
    let read = node.seq.account_payouts(TRADER, None, ALL);
    assert_eq!(read.payouts[0].withdrawal.as_ref(), Some(&second), "the rotated authorization is in effect");

    // So does a holder's request once the authorization expired.
    node.clock.advance(Duration::from_secs(2 * crate::withdrawals::LIFETIME_SECONDS));
    node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
    let expires_at = second.authorization.as_ref().unwrap().expires_at;
    assert_eq!(node.seq.request_reissue(TRADER, 1, expires_at, None), Reissue::Queued);
    let third = sign(&mut node, &rotated);
    assert_ne!(third, second);
    let read = node.seq.account_payouts(TRADER, None, ALL);
    assert_eq!(read.payouts[0].withdrawal.as_ref(), Some(&third), "a reissue replaces the read");
}

#[test]
fn a_paid_approval_is_absent_and_an_unknown_account_is_empty() {
    let mut node = Node::new("payouts-read-paid");
    let cp = node.approve();
    let mut evidence = node.evidence(&cp);
    sign(&mut node, &evidence);
    assert_eq!(node.seq.account_payouts(TRADER, None, ALL).payouts.len(), 1);

    evidence.approvals = vec![status(&node, 1, true)];
    node.seq.release_payouts(&evidence).unwrap();
    let paid = node.seq.account_payouts(TRADER, None, ALL);
    assert!(paid.loaded && paid.payouts.is_empty(), "a claimed approval leaves the read");

    let stranger = node.seq.account_payouts(AccountId([9; 20]), None, ALL);
    assert!(stranger.loaded && stranger.payouts.is_empty() && stranger.next.is_none());
}

#[test]
fn pages_run_oldest_first_and_name_where_the_next_begins() {
    let mut node = Node::new("payouts-read-pages");
    node.seq.sequence(deposit(1_000, "payouts-read-pages-deposit")).unwrap().unwrap();
    for _ in 0..5 {
        node.seq.sequence(withdraw(1)).unwrap().unwrap();
        while node._queued.try_recv().is_ok() {}
    }
    let nonces = |page: &AccountPayouts| page.payouts.iter().map(|view| view.nonce).collect::<Vec<_>>();
    let first = node.seq.account_payouts(TRADER, None, 2);
    assert_eq!((nonces(&first), first.next), ([1, 2].to_vec(), Some(2)));
    let second = node.seq.account_payouts(TRADER, first.next, 2);
    assert_eq!((nonces(&second), second.next), ([3, 4].to_vec(), Some(4)));
    let last = node.seq.account_payouts(TRADER, second.next, 2);
    assert_eq!((nonces(&last), last.next), ([5].to_vec(), None));
    let exact = node.seq.account_payouts(TRADER, Some(2), 3);
    assert_eq!((nonces(&exact), exact.next), ([3, 4, 5].to_vec(), None), "a full final page has no cursor");
}

#[test]
fn a_read_without_a_checkpoint_gate_never_claims_to_be_loaded() {
    let mut node = Node::new("payouts-read-ungated");
    node.approve();
    node.seq.gate = None;
    let read = node.seq.account_payouts(TRADER, None, ALL);
    assert!(!read.loaded && read.payouts.is_empty() && read.next.is_none());
}

#[test]
fn loaded_stays_false_until_a_restart_has_loaded_every_approval() {
    let mut node = Node::new("payouts-read-restart");
    node.seq.sequence(deposit(10_000, "payouts-read-restart-deposit")).unwrap().unwrap();
    let total = BACKLOG_WINDOW + 3;
    for _ in 0..total {
        node.seq.sequence(withdraw(1)).unwrap().unwrap();
        while node._queued.try_recv().is_ok() {}
    }
    let cp = node.last_checkpoint();
    let mut evidence = node.evidence(&cp);
    evidence.approvals = vec![status(&node, 1, false)];
    let issued = sign(&mut node, &evidence);

    let mut recovered = node.restart(&node.last_checkpoint());
    let early = recovered.seq.account_payouts(TRADER, None, ALL);
    assert!(!early.loaded, "{} approvals wait unloaded, so absence proves nothing", total - BACKLOG_WINDOW);
    assert_eq!(early.payouts.len(), BACKLOG_WINDOW);
    assert_eq!(early.payouts[0].withdrawal.as_ref(), Some(&issued), "recovery restores the logged authorization");
    assert!(early.payouts[1].withdrawal.is_none());
    assert!(early.payouts.iter().all(|view| view.nonce <= BACKLOG_WINDOW as u64), "the oldest load first");

    // Custody reports everything paid but the first; each swept window loads the next. `loaded`
    // turns true once the last window is loaded, which custody has not yet reported on.
    let mut sweep = |recovered: &mut Node| {
        evidence.approvals = recovered
            .seq
            .gate
            .as_ref()
            .unwrap()
            .payouts
            .values()
            .filter(|payout| payout.recovered)
            .map(|payout| status(recovered, payout.approval.nonce, payout.approval.nonce != 1))
            .collect();
        recovered.seq.release_payouts(&evidence).unwrap();
    };
    let mut sweeps = 0;
    while !recovered.seq.account_payouts(TRADER, None, ALL).loaded {
        sweep(&mut recovered);
        sweeps += 1;
        assert!(sweeps < 8, "the sweep makes progress");
    }
    let unswept = recovered.seq.account_payouts(TRADER, None, ALL);
    assert!(unswept.payouts.len() > 1, "the last window is listed until custody reports on it");
    sweep(&mut recovered);
    let done = recovered.seq.account_payouts(TRADER, None, ALL);
    assert!(done.loaded);
    assert_eq!(done.payouts.iter().map(|view| view.nonce).collect::<Vec<_>>(), [1], "only the unpaid one remains");
    assert_eq!(done.payouts[0].withdrawal.as_ref(), Some(&issued));
}
