//! A checkpoint waits while the last signed one is not yet final, unless it must not: an approval
//! gets the next one, the gate's alerts still run, and a handover's or a drain's goes out regardless.

use std::time::Duration;

use engine_core::types::AccountId;
use tokio::sync::mpsc::Receiver;

use super::*;
use crate::checkpoints::{ApprovalId, ApprovalStatus, LifecycleEvidence, Registry, Work};
use crate::clock::test_clock::TestClock;
use crate::log::{Checkpoint, CheckpointParent};
use crate::sequencer::checkpoints::HandoverGate;
use crate::sequencer::{Request, SequencerConfig};
use crate::test_support::sample_around;

const STALLED_AFTER: Duration = Duration::from_secs(30 * 60);

#[test]
fn the_next_checkpoint_waits_until_the_last_signed_is_final() {
    let mut node = Node::new("hold-back-cadence");
    let first = node.tick_signs();
    for _ in 0..5 {
        node.tick_holds();
    }
    // Neither an observation that failed nor a report of an older checkpoint frees the next.
    node.seq.checkpoint_status(None, None).unwrap();
    node.seq.checkpoint_status(Some(first.seq - 1), Some(Health::Healthy)).unwrap();
    node.tick_holds();
    node.seq.checkpoint_status(Some(first.seq), Some(Health::Healthy)).unwrap();
    let next = node.tick_signs();
    let parent = next.parent.unwrap();
    assert_eq!((parent.seq, parent.root.to_string()), (first.seq, first.root));
    // A stale report after a newer one moves nothing back.
    node.seq.checkpoint_status(Some(next.seq), Some(Health::Healthy)).unwrap();
    node.seq.checkpoint_status(Some(first.seq), Some(Health::Healthy)).unwrap();
    node.tick_signs();
}

#[test]
fn an_approval_with_nothing_on_its_way_signs_at_once() {
    let mut node = Node::new("hold-back-approval-idle");
    node.seq.sequence(withdraw(1)).unwrap().unwrap();
    let work = node.queued.try_recv().expect("the approval's own checkpoint");
    assert_eq!(work.checkpoint.seq, node.seq.last_seq().unwrap());
    assert!(node.seq.gate.as_ref().unwrap().unproved.is_empty());
}

#[test]
fn approvals_held_behind_a_checkpoint_share_the_next_with_no_further_command() {
    let mut node = Node::new("hold-back-approval-held");
    let first = node.tick_signs();
    for _ in 0..3 {
        node.seq.sequence(withdraw(1)).unwrap().unwrap();
    }
    assert!(node.queued.try_recv().is_err(), "the approvals wait for the one on its way");
    assert_eq!(node.seq.gate.as_ref().unwrap().unproved.len(), 3);
    assert!(!signed_withdrawal(&node));

    // The evidence that the checkpoint is final signs the next, covering all three.
    let last = node.seq.last_seq().unwrap();
    node.final_at(&first);
    let covering = node.queued.try_recv().expect("the approvals' checkpoint").checkpoint;
    assert_eq!(covering.seq, last, "it covers the commands so far, with none after them");
    assert!(node.queued.try_recv().is_err(), "three approvals, one checkpoint");
    assert!(node.seq.gate.as_ref().unwrap().unproved.is_empty());
    assert!(!signed_withdrawal(&node), "and no authorization until that one is final");

    node.final_at(&covering);
    node.seq.flush().unwrap();
    let nonces: Vec<u64> = std::iter::from_fn(|| node.signed.try_recv().ok()).map(|signed| signed.nonce).collect();
    assert_eq!(nonces, [1, 2, 3], "the authorizations follow it, oldest first");
}

#[test]
fn a_held_attempt_still_runs_the_gate_and_tells_the_handover_when_signing_can_resume() {
    let (signing, signable) = watch::channel(true);
    let mut node = Node::with("hold-back-withheld", Some(signing));
    node.tick_signs();
    let lines = crate::alerts::capture(|| {
        node.seq.checkpoint_status(None, Some(Health::Diverged)).unwrap();
        node.tick_holds();
        assert!(!*signable.borrow(), "the attempt behind a hold finds the divergence");
        node.seq.checkpoint_status(None, Some(Health::Healthy)).unwrap();
        node.tick_holds();
        assert!(*signable.borrow(), "and finds it cleared");
    });
    let key = "checkpoint_withheld{cause=collateral}".to_owned();
    assert_eq!(
        crate::alerts::pages_for(lines),
        [(crate::alerts::Status::Firing, key.clone()), (crate::alerts::Status::Resolved, key)]
    );
}

#[test]
fn a_handovers_final_checkpoint_is_not_held_back() {
    let mut node = Node::new("hold-back-handover");
    node.tick_signs();
    node.seq.sequence(Command::Tick).unwrap().unwrap();
    assert!(node.queued.try_recv().is_err());
    assert!(matches!(node.seq.handover_checkpoint().unwrap(), HandoverGate::Admitted));
    assert_eq!(node.queued.try_recv().unwrap().checkpoint.seq, node.seq.last_seq().unwrap());
}

#[tokio::test]
async fn a_drains_final_checkpoint_is_not_held_back() {
    let mut node = Node::new("hold-back-drain");
    node.tick_signs();
    node.seq.sequence(Command::Tick).unwrap().unwrap();
    assert!(node.queued.try_recv().is_err());
    let Node { seq, mut queued, .. } = node;
    let last = seq.last_seq().unwrap();
    let (_, closed) = mpsc::channel::<Request>(1);
    seq.run(closed).await.unwrap();
    assert_eq!(queued.try_recv().unwrap().checkpoint.seq, last);
}

#[test]
fn a_restart_holds_until_its_recovered_checkpoints_are_final_and_then_covers_its_approvals() {
    let mut node = Node::new("hold-back-restart");
    let first = node.tick_signs();
    node.seq.sequence(withdraw(1)).unwrap().unwrap();
    node.seq.checkpoint().unwrap();
    let second = node.queued.try_recv().unwrap().checkpoint;
    node.seq.sequence(withdraw(1)).unwrap().unwrap();
    node.seq.flush().unwrap();
    let Node { seq, dir, clock, anchor, .. } = node;
    drop(seq);
    let (sink, mut queued) = mpsc::channel(64);
    let (withdrawals, _signed) = mpsc::unbounded_channel();
    let registry = Registry::default();
    let parent = CheckpointParent { has_checkpoint: true, seq: anchor.seq, root: anchor.root.parse().unwrap() };
    let cfg = SequencerConfig { checkpoint_parent: parent, ..config(&dir, &clock, sink, registry.clone(), None) };
    let mut seq =
        Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(16).0, withdrawals).unwrap();
    assert_eq!(seq.take_recovery_checkpoints().len(), 3, "the anchor and both unposted checkpoints");
    assert_eq!(registry.lock().unwrap().range(anchor.seq + 1..).count(), 2);
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.checkpoint_status(Some(anchor.seq), Some(Health::Healthy)).unwrap();
    assert!(seq.gate.as_ref().unwrap().unproved.len() >= 2, "the recovered approvals still need proofs");

    for final_seq in [first.seq, second.seq] {
        seq.sequence(Command::Tick).unwrap().unwrap();
        seq.prove_loaded().unwrap();
        assert!(queued.try_recv().is_err(), "a recovered checkpoint before seq {final_seq} is not yet final");
        seq.checkpoint_status(Some(final_seq), Some(Health::Healthy)).unwrap();
    }
    seq.prove_loaded().unwrap();
    let next = queued.try_recv().expect("the recovered chain is final, so the proofs get a checkpoint").checkpoint;
    assert_eq!(next.parent.unwrap().seq, second.seq, "it extends the recovered chain");
    assert!(seq.gate.as_ref().unwrap().unproved.is_empty());
}

#[test]
fn a_recovered_approval_observed_paid_behind_a_hold_needs_no_proof() {
    let mut node = Node::new("hold-back-recovered-paid");
    node.tick_signs();
    for _ in 0..2 {
        node.seq.sequence(withdraw(1)).unwrap().unwrap();
    }
    node.seq.checkpoint().unwrap();
    let last = node.queued.try_recv().unwrap().checkpoint;
    // A command past the last checkpoint, so recovery cannot prove against that one.
    node.seq.sequence(Command::Tick).unwrap().unwrap();
    node.seq.flush().unwrap();
    let Node { seq, dir, clock, anchor, .. } = node;
    drop(seq);
    let (sink, mut queued) = mpsc::channel(64);
    let parent = CheckpointParent { has_checkpoint: true, seq: anchor.seq, root: anchor.root.parse().unwrap() };
    let cfg = SequencerConfig { checkpoint_parent: parent, ..config(&dir, &clock, sink, Registry::default(), None) };
    let (withdrawals, _signed) = mpsc::unbounded_channel();
    let mut seq =
        Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(16).0, withdrawals).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.checkpoint_status(Some(anchor.seq), Some(Health::Healthy)).unwrap();
    assert_eq!(seq.gate.as_ref().unwrap().unproved.len(), 2);

    // Behind the hold, custody reports the older approval paid: only the other still needs a proof.
    seq.sequence(Command::Tick).unwrap().unwrap();
    let evidence = LifecycleEvidence { approvals: statuses(&seq, &[1]), ..evidence_at(&seq, &anchor) };
    seq.custody_status(Some(anchor.seq), Some(Health::Healthy), Some(evidence)).unwrap();
    assert!(queued.try_recv().is_err(), "the recovered checkpoints are not yet final");
    assert_eq!(seq.gate.as_ref().unwrap().unproved.len(), 1);

    // The checkpoint the hold released covers it, with no stale identity left to prove.
    let evidence = LifecycleEvidence { approvals: statuses(&seq, &[1]), ..evidence_at(&seq, &last) };
    seq.custody_status(Some(last.seq), Some(Health::Healthy), Some(evidence)).unwrap();
    assert_eq!(queued.try_recv().expect("the next checkpoint").checkpoint.parent.unwrap().seq, last.seq);
    let gate = seq.gate.as_ref().unwrap();
    assert!(gate.unproved.is_empty());
    assert_eq!(gate.payouts.keys().map(|(_, nonce)| *nonce).collect::<Vec<_>>(), [2], "the paid one is gone");
}

#[test]
fn a_hold_is_a_backlog_only_past_the_stall_bound_and_a_final_checkpoint_restarts_the_bound() {
    let mut node = Node::new("hold-back-backlog");
    let first = node.tick_signs();
    let lines = crate::alerts::capture(|| {
        // A finality delay and the next: each inside the bound, so nothing fires.
        node.tick_holds();
        node.advance(STALLED_AFTER - Duration::from_secs(1));
        node.tick_holds();
        node.seq.checkpoint_status(Some(first.seq), Some(Health::Healthy)).unwrap();
        let second = node.tick_signs();
        node.tick_holds();
        node.advance(STALLED_AFTER - Duration::from_secs(1));
        node.tick_holds();
        // Past the bound with none final: one episode however many attempts it spans.
        node.advance(Duration::from_secs(1));
        for _ in 0..3 {
            node.tick_holds();
        }
        node.seq.checkpoint_status(Some(second.seq), Some(Health::Healthy)).unwrap();
        node.tick_signs();
    });
    let key = "checkpoint_backlog{}".to_owned();
    assert_eq!(
        crate::alerts::pages_for(lines),
        [(crate::alerts::Status::Firing, key.clone()), (crate::alerts::Status::Resolved, key)]
    );
}

/// What a held checkpoint costs the sequencer's thread once it signs: the root over everything
/// changed during the hold, here every one of 100k accounts, and the proofs of 1,000 approvals.
#[test]
#[ignore = "signed checkpoint cost after a hold at 100k accounts: run in a quiet CPU window"]
fn held_checkpoint_cost_at_100k_accounts() {
    const ACCOUNTS: u32 = 100_000;
    let account = |index: u32| {
        let mut bytes = [1; 20];
        bytes[16..].copy_from_slice(&index.to_be_bytes());
        AccountId(bytes)
    };
    let credit = |seq: &mut Sequencer, index: u32, round: u32| {
        let chain_ref = format!("cost-{round}-{index}");
        let cmd = Command::Deposit { account: account(index), amount: Fixed::from_int(10), chain_ref };
        seq.sequence(cmd).unwrap().unwrap();
        if index.is_multiple_of(1024) {
            seq.flush().unwrap();
        }
    };
    let mut node = Node::new("hold-back-cost");
    node.seq.checkpoint_every = u64::MAX;
    for index in 0..ACCOUNTS {
        credit(&mut node.seq, index, 0);
    }
    node.seq.checkpoint().unwrap();
    let first = node.queued.try_recv().unwrap().checkpoint;
    // A finality delay's changes behind the hold.
    for index in 0..ACCOUNTS {
        credit(&mut node.seq, index, 1);
    }
    for index in 0..1_000 {
        let withdraw = Command::Withdraw { account: account(index), amount: Fixed::ONE, auth: None };
        assert_eq!(approved_count(&node.seq.sequence(withdraw).unwrap().unwrap()), 1);
    }
    assert_eq!(node.seq.gate.as_ref().unwrap().unproved.len(), 1_000);
    node.seq.checkpoint_status(Some(first.seq), Some(Health::Healthy)).unwrap();
    let started = std::time::Instant::now();
    node.seq.prove_loaded().unwrap();
    let elapsed = started.elapsed();
    node.queued.try_recv().expect("the held checkpoint signs");
    let work = node.seq.engine.last_root_work().unwrap();
    println!("HELD CHECKPOINT {elapsed:?}: {} leaves, {} nodes, 1000 approval proofs", work.leaves, work.nodes);
}

// ── Helpers ──

fn approved_count(events: &[engine_core::event::Event]) -> usize {
    events.iter().filter(|event| matches!(event, engine_core::event::Event::WithdrawalApproved { .. })).count()
}

/// Custody's answer for every approval: `paid` are the nonces it reports claimed.
fn statuses(seq: &Sequencer, paid: &[u64]) -> Vec<ApprovalStatus> {
    seq.engine
        .withdrawal_approvals()
        .map(|approval| ApprovalStatus {
            approval: ApprovalId {
                account: approval.account,
                nonce: approval.nonce,
                approved_seq: approval.approved_seq,
            },
            claimed: paid.contains(&approval.nonce),
        })
        .collect()
}

/// A finalized observation of `checkpoint` under this enclave's authority, about no approval.
fn evidence_at(seq: &Sequencer, checkpoint: &Checkpoint) -> LifecycleEvidence {
    LifecycleEvidence {
        height: 1,
        block_hash: alloy::primitives::B256::repeat_byte(1),
        signer: AccountId(seq.key.address()),
        signer_epoch: 1,
        withdrawals_paused: false,
        total_withdrawn: Fixed::ZERO,
        checkpoint_seq: Some(checkpoint.seq),
        checkpoint_root: Some(checkpoint.root.clone()),
        approvals: Vec::new(),
    }
}

/// Whether any withdrawal authorization reached the log.
fn signed_withdrawal(node: &Node) -> bool {
    log_entries(&node.dir).iter().any(|entry| matches!(entry, LogEntry::Withdrawal(_)))
}

/// A gated sequencer's config, with a checkpoint every command and the poster's registry.
fn config<'a>(
    dir: &'a std::path::Path,
    clock: &Arc<TestClock>,
    sink: mpsc::Sender<Work>,
    registry: Registry,
    signing: Option<watch::Sender<bool>>,
) -> SequencerConfig<'a> {
    SequencerConfig {
        clock: clock.clone(),
        checkpoints: Some(sink),
        checkpoint_every: 1,
        segment_commands: u64::MAX,
        checkpoint_registry: Some(registry),
        checkpoint_stalled_after: STALLED_AFTER,
        checkpoint_signing: signing,
        approval_registry: Some(Default::default()),
        ..sequencer_config(dir, watch::channel(Manifest::default()).0)
    }
}

/// A funded account on a gated sequencer whose bootstrap checkpoints are final, with nothing on its
/// way to the vault.
struct Node {
    seq: Sequencer,
    queued: Receiver<Work>,
    signed: mpsc::UnboundedReceiver<crate::sequencer::SignedWithdrawal>,
    clock: Arc<TestClock>,
    dir: std::path::PathBuf,
    anchor: Checkpoint,
}

impl Node {
    fn new(name: &str) -> Self {
        Self::with(name, None)
    }

    fn with(name: &str, signing: Option<watch::Sender<bool>>) -> Self {
        let dir = scratch_dir(name);
        let clock = Arc::new(TestClock::default());
        let (sink, mut queued) = mpsc::channel(64);
        let (withdrawals, signed) = mpsc::unbounded_channel();
        let cfg = config(&dir, &clock, sink, Registry::default(), signing);
        let mut seq =
            Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(4096).0, withdrawals).unwrap();
        seq.activate_legacy_protocol().unwrap();
        seq.genesis(&dev_config().genesis).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        seq.sequence(crate::test_support::empty_custody_frontier()).unwrap();
        seq.checkpoint().unwrap();
        seq.checkpoint_status(seq.last_seq(), Some(Health::Healthy)).unwrap();
        seq.sequence(deposit(1_000, "hold-back-funds")).unwrap().unwrap();
        seq.checkpoint().unwrap();
        let anchor = std::iter::from_fn(|| queued.try_recv().ok()).last().expect("the funding checkpoint").checkpoint;
        seq.checkpoint_status(Some(anchor.seq), Some(Health::Healthy)).unwrap();
        seq.flush().unwrap();
        Self { seq, queued, signed, clock, dir, anchor }
    }

    /// Moves the clock on, with a time sample so commands are still admitted.
    fn advance(&mut self, by: Duration) {
        self.clock.advance(by);
        self.seq.trust(sample_around(self.clock.as_ref())).unwrap();
    }

    /// Sequences a command and returns the checkpoint it signed.
    fn tick_signs(&mut self) -> Checkpoint {
        self.seq.sequence(Command::Tick).unwrap().unwrap();
        self.queued.try_recv().expect("a checkpoint follows the command").checkpoint
    }

    /// Sequences a command that signs no checkpoint.
    fn tick_holds(&mut self) {
        self.seq.sequence(Command::Tick).unwrap().unwrap();
        assert!(self.queued.try_recv().is_err(), "the checkpoint is held back");
    }

    /// Reports `checkpoint` final with evidence that every approval is unpaid, as the worker does
    /// on each tick.
    fn final_at(&mut self, checkpoint: &Checkpoint) {
        let evidence = LifecycleEvidence { approvals: statuses(&self.seq, &[]), ..evidence_at(&self.seq, checkpoint) };
        self.seq.custody_status(Some(checkpoint.seq), Some(Health::Healthy), Some(evidence)).unwrap();
    }
}
