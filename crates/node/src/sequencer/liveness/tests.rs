//! The sequencer under the liveness rule: commands wait on its gate, a checkpoint waits for a
//! member's confirmation of its seq, and the hold it signs held heads under.

use std::sync::Arc;

use engine_core::command::Command;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::liveness::Gate;
use crate::log::{LogEntry, Manifest};
use crate::log_heads::{Head, Hold, Publisher};
use crate::sequencer::{Request, Sequencer};
use crate::tee::EnclaveKey;
use crate::test_support::{CHECKPOINT_EVERY, log_entries, scratch_dir, sequencer_config, trusted_now};

#[tokio::test]
async fn a_checkpoint_waits_for_a_member_to_confirm_its_seq_and_nothing_is_stamped_past_it() {
    let dir = scratch_dir("seq-liveness-checkpoint");
    let gate = Gate::closed();
    gate.fence(false);
    gate.checked(true);
    let (seq, _hold) = sequencer(&dir, gate.clone());
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(seq.run(rx));
    // Commands up to the first checkpoint's seq are stamped; its checkpoint waits for a confirmation.
    let mut stamped = 0;
    while submit(&tx).await {
        stamped += 1;
        assert!(stamped <= CHECKPOINT_EVERY, "a checkpoint is due within {CHECKPOINT_EVERY} commands");
    }
    assert!(!submit(&tx).await, "nothing is stamped past the waiting checkpoint's seq");
    assert!(!log_entries(&dir).iter().any(|entry| matches!(entry, LogEntry::Checkpoint(_))), "not signed yet");
    let due = last_seq(&tx).await;
    gate.confirm(due - 1);
    assert!(!submit(&tx).await, "a confirmation short of its seq releases nothing");
    gate.confirm(due);
    assert!(!submit(&tx).await, "the request that finds the confirmation is refused, then the checkpoint written");
    assert!(submit(&tx).await, "sequencing resumes");
    drop(tx);
    task.await.unwrap().unwrap();
    let checkpoints: Vec<u64> = log_entries(&dir)
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint.seq),
            _ => None,
        })
        .collect();
    assert_eq!(checkpoints.first(), Some(&due), "the checkpoint signed is at the confirmed seq");
    assert!(checkpoints.iter().all(|seq| *seq <= due + 1), "{checkpoints:?}");
}

#[tokio::test]
async fn a_closed_gate_stamps_nothing_and_an_open_one_without_the_rule_confirms_everything() {
    let dir = scratch_dir("seq-liveness-gate");
    let gate = Gate::closed();
    let (seq, _hold) = sequencer(&dir, gate.clone());
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(seq.run(rx));
    assert!(!submit(&tx).await, "before the rule's first read nothing is stamped");
    gate.open();
    for _ in 0..2 * CHECKPOINT_EVERY {
        assert!(submit(&tx).await);
    }
    drop(tx);
    task.await.unwrap().unwrap();
    assert!(log_entries(&dir).iter().any(|entry| matches!(entry, LogEntry::Checkpoint(_))), "no checkpoint waits");
}

#[tokio::test]
async fn a_benchmark_sync_hold_is_published_as_the_hold_heads_are_signed_under() {
    let dir = scratch_dir("seq-liveness-hold");
    let (mut seq, hold) = sequencer(&dir, Gate::default());
    seq.benchmark_sync = true;
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(seq.run(rx));
    submit(&tx).await;
    assert_eq!(*hold.borrow(), Hold::BenchmarkSync);
    drop(tx);
    task.await.unwrap().unwrap();
}

// ── Helpers ──

/// A trusted sequencer over `dir` whose head publisher shares `gate`, with the hold it publishes.
fn sequencer(dir: &std::path::Path, gate: Gate) -> (Sequencer, watch::Receiver<Hold>) {
    let (stream, _) = broadcast::channel(16);
    let (withdrawals, _) = mpsc::unbounded_channel();
    let (manifest, _) = watch::channel(Manifest::default());
    let mut seq =
        Sequencer::recover(sequencer_config(dir, manifest), Arc::new(EnclaveKey::generate()), stream, withdrawals)
            .unwrap();
    let mut publisher = Publisher::for_test(watch::channel(Head::default()).0, Default::default());
    publisher.gate = gate;
    let hold = publisher.hold.subscribe();
    seq.log_heads = Some(publisher);
    seq.trust(trusted_now()).unwrap();
    (seq, hold)
}

/// The sequencer's last seq.
async fn last_seq(tx: &mpsc::Sender<Request>) -> u64 {
    let (reply, answer) = oneshot::channel();
    let query = Box::new(move |engine: &engine_core::Engine| {
        let _ = reply.send(engine.last_seq());
    });
    tx.send(Request::Query(query)).await.unwrap();
    answer.await.unwrap().unwrap()
}

/// Submits a tick and reports whether it was stamped.
async fn submit(tx: &mpsc::Sender<Request>) -> bool {
    let (reply, replied) = oneshot::channel();
    tx.send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).await.unwrap();
    replied.await.is_ok()
}
