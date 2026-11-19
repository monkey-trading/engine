//! A payout's first proof comes from the root worker once the tree is detached: after the checkpoint
//! that covers its approval, and, for approvals a recovery window loads while the last checkpoint
//! already covers the state, from that checkpoint, with no new one written.

use engine_core::eip712::B256;
use engine_core::merkle::WithdrawalApprovalProof;
use engine_core::withdrawal::WithdrawalApproval;

use super::*;
use crate::checkpoints::{ApprovalId, ApprovalStatus, LifecycleEvidence};
use crate::log::{CheckpointDomain, CheckpointParent};
use crate::sequencer::payouts::BACKLOG_WINDOW;
use crate::test_support::{TRADER, deposit, withdraw};

/// How long the settle must still be waiting while the worker holds the proof it waits for.
const HELD: Duration = Duration::from_millis(500);

/// Custody's finalized view: the checkpoint at `checkpoint` posted, signed by `signer`, and the
/// trader's `approvals`, each with its seq and whether it was claimed.
fn evidence(signer: AccountId, checkpoint: &Checkpoint, approvals: &[(u64, u64, bool)]) -> LifecycleEvidence {
    LifecycleEvidence {
        height: 10,
        block_hash: alloy::primitives::B256::repeat_byte(1),
        signer,
        signer_epoch: 1,
        withdrawals_paused: false,
        total_withdrawn: Fixed::ZERO,
        checkpoint_seq: Some(checkpoint.seq),
        checkpoint_root: Some(checkpoint.root.clone()),
        approvals: approvals
            .iter()
            .map(|&(nonce, approved_seq, claimed)| ApprovalStatus {
                approval: ApprovalId { account: TRADER, nonce, approved_seq },
                claimed,
            })
            .collect(),
    }
}

/// Custody's report of `evidence` with collateral reconciled at its checkpoint.
fn observed(evidence: LifecycleEvidence) -> Request {
    let posted = evidence.checkpoint_seq;
    Request::CheckpointStatus { posted, latest: None, health: Some(Health::Healthy), lifecycle: Some(evidence) }
}

/// Asserts `signed` is the trader's approval `nonce` at `approved_seq`, authorized against
/// `checkpoint` with a proof of it that holds against that checkpoint's root.
fn assert_proved(signed: &SignedWithdrawal, nonce: u64, approved_seq: u64, checkpoint: &Checkpoint) {
    let authorization = signed.authorization.as_ref().expect("a payout carries its authorization");
    let root: B256 = checkpoint.root.parse().expect("a logged root parses");
    assert_eq!((signed.account, signed.nonce), (TRADER, nonce));
    assert_eq!((authorization.checkpoint_seq, authorization.checkpoint_root), (checkpoint.seq, root));
    let approval = WithdrawalApproval { account: TRADER, amount: signed.amount, nonce, approved_seq };
    let proof =
        WithdrawalApprovalProof { seq: checkpoint.seq, root, approval, siblings: authorization.siblings.clone() };
    assert!(proof.verify(root), "the payout's proof holds against its checkpoint's root");
}

#[tokio::test]
async fn an_approvals_payout_is_signed_with_the_workers_proof_at_its_checkpoint() {
    let (work, mut queued) = mpsc::channel(64);
    let mut node = Node::new("roots-payout-proof", Some(work));
    node.seq.sequence(deposit(100, "roots-payout-deposit")).expect("applies").expect("admitted");
    node.seq.flush().expect("the log flushes");
    while queued.try_recv().is_ok() {}
    let signer = AccountId(node.seq.key.address());
    let mut running = node.run(Vec::new());

    let events = running.submit(withdraw(40)).await;
    assert!(events.iter().any(|event| matches!(event, Event::WithdrawalApproved { nonce: 1, .. })), "{events:?}");
    let posted = timeout(WAIT, queued.recv()).await.expect("the approval is checkpointed").expect("the gate posts it");
    let checkpoint = posted.checkpoint;
    running.send(observed(evidence(signer, &checkpoint, &[(1, checkpoint.seq, false)]))).await;

    let signed = timeout(WAIT, running.signed.recv()).await.expect("the payout is signed").expect("custody receives");
    assert_proved(&signed, 1, checkpoint.seq, &checkpoint);
    assert_eq!(signed.amount, Fixed::from_int(40));
    let Stopped { ended, .. } = running.stop().await;
    ended.expect("the sequencer drains");
}

#[tokio::test]
async fn approvals_a_recovery_window_loads_are_proved_at_the_checkpoint_that_covers_them() {
    let (work, mut queued) = mpsc::channel(64);
    let mut node = Node::new("roots-payout-window", Some(work.clone()));
    node.seq.sequence(deposit(10_000, "roots-window-deposit")).expect("applies").expect("admitted");
    let last = BACKLOG_WINDOW as u64 + 1;
    for _ in 0..last {
        node.seq.sequence(withdraw(1)).expect("applies").expect("admitted");
        while queued.try_recv().is_ok() {}
    }
    node.seq.flush().expect("the log flushes");
    let covering = checkpoints(&log_entries(&node.dir)).last().map(|checkpoint| (*checkpoint).clone()).expect("one");
    assert_eq!(Some(covering.seq), node.seq.last_seq(), "the last approval's checkpoint covers the state");

    // A same-key restart from that checkpoint loads the oldest window, proved inline, and leaves the
    // last approval for the next window.
    let Node { seq, dir, clock, .. } = node;
    let key = seq.key.clone();
    drop(seq);
    let parent = CheckpointParent { has_checkpoint: true, seq: covering.seq, root: covering.root.parse().unwrap() };
    let cfg = SequencerConfig { checkpoint_parent: parent, ..Node::config(&dir, &clock, Some(work)) };
    let domain = CheckpointDomain { chain_id: cfg.chain_id, vault: cfg.vault };
    let signer = AccountId(key.address());
    let log = Sequencer::read_log_for_recovery(&dir, Some(domain), Some(signer), Some(parent), |_| {}).unwrap();
    let (withdrawals, signed) = mpsc::unbounded_channel();
    let mut seq = Sequencer::recover_from(log, cfg, key, broadcast::channel(4096).0, withdrawals).unwrap();
    seq.trust(sample_around(clock.as_ref())).expect("trusted time");
    seq.checkpoint_status(Some(covering.seq), Some(Health::Healthy)).expect("collateral reconciles");
    let gate = seq.gate.as_ref().expect("gated");
    assert_eq!((gate.payouts.len(), gate.backlog.len()), (BACKLOG_WINDOW, 1), "the last approval waits unloaded");
    let approved = |nonce| seq.engine.withdrawal_approval(TRADER, nonce).expect("approved").approved_seq;
    let window: Vec<(u64, u64, bool)> = (1..last).map(|nonce| (nonce, approved(nonce), true)).collect();
    let last_seq = approved(last);
    let node = Node { seq, dir, clock, gate: None, signed };
    let mut running = node.run(Vec::new());

    // Custody reports the loaded window paid, so the last approval loads, and then reports it unpaid.
    running.send(observed(evidence(signer, &covering, &window))).await;
    running.send(observed(evidence(signer, &covering, &[(last, last_seq, false)]))).await;
    let signed = timeout(WAIT, running.signed.recv()).await.expect("the payout is signed").expect("custody receives");
    assert_proved(&signed, last, last_seq, &covering);

    let Stopped { ended, entries, .. } = running.stop().await;
    ended.expect("the sequencer drains");
    assert_eq!(checkpoints(&entries).last().map(|checkpoint| checkpoint.seq), Some(covering.seq), "no new checkpoint");
}

#[test]
fn a_payout_release_waits_only_for_proofs_at_checkpoints_the_vault_has_posted() {
    let mut node = Node::new("roots-payout-settle", None);
    node.seq.sequence(deposit(100, "roots-settle-deposit")).expect("applies").expect("admitted");
    let events = node.seq.sequence(withdraw(40)).expect("applies").expect("admitted");
    assert!(events.iter().any(|event| matches!(event, Event::WithdrawalApproved { nonce: 1, .. })), "{events:?}");
    node.seq.checkpoint().expect("the approval is checkpointed");
    node.seq.flush().expect("the log flushes");
    node.seq.start_root_worker().expect("the worker takes the tree");
    let posted = node.seq.last_seq().expect("the approval is checkpointed");
    // A proof asked at the posted checkpoint is waited for, held back on the worker until the settle has
    // started, and one at a later checkpoint the worker has yet to answer, as behind a rollover's
    // writes, is not.
    let release = node.seq.roots.worker.hold().expect("the worker takes the hold");
    node.seq.prove_payouts(posted, vec![(TRADER, 1)]).expect("the worker takes the ask");
    node.seq.roots.proving.insert(posted + 1, 1);
    let (settled, done) = std::sync::mpsc::channel();
    let settling = std::thread::spawn(move || {
        let outcome = node.seq.settle_payout_proofs(Some(posted));
        _ = settled.send(());
        (node, outcome)
    });
    let held = done.recv_timeout(HELD);
    assert!(held.is_err(), "the release returned before the posted checkpoint's proof was answered");
    drop(release);
    done.recv_timeout(WAIT).expect("the release waits for no proof at a checkpoint the vault has not posted");
    let (node, outcome) = settling.join().expect("the settle returns");
    outcome.expect("the worker answers");
    assert_eq!(
        node.seq.roots.proving,
        BTreeMap::from([(posted + 1, 1)]),
        "only the posted checkpoint's proof is taken"
    );
}
