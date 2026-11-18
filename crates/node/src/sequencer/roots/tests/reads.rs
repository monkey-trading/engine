//! Proof reads answer only from a checkpoint the log holds durably: at once from a start whose tree is
//! at the log's last checkpoint, and otherwise, as after a start whose tree is past it or after an
//! activation that discards the tree, from the next checkpoint once its batch is durable.

use engine_core::eip712::B256;
use engine_core::protocol::ProtocolVersion;

use super::*;
use crate::sequencer::fixture_tests::replay_trusted;
use crate::test_support::{TRADER, deposit};

/// Asserts `proof` is the trader's at `checkpoint`, and holds against its root.
fn assert_at(proof: Option<AccountProof>, checkpoint: &Checkpoint) {
    let proof = proof.expect("the trader's proof is answered");
    let root: B256 = checkpoint.root.parse().expect("a logged root parses");
    assert_eq!((proof.account, proof.seq, proof.root), (TRADER, checkpoint.seq, root));
    assert!(proof.verify(root), "the proof holds against the checkpoint's root");
}

#[tokio::test]
async fn a_start_whose_tree_is_at_the_logs_last_checkpoint_answers_proofs_at_once() {
    let mut node = Node::new("roots-read-at-checkpoint", None);
    node.seq.sequence(deposit(100, "roots-read-deposit")).expect("applies").expect("admitted");
    node.seq.checkpoint().expect("checkpointed");
    node.seq.flush().expect("the log flushes");
    let entries = log_entries(&node.dir);
    let last = (*checkpoints(&entries).last().expect("a checkpoint")).clone();
    assert_eq!(Some(last.seq), node.seq.last_seq());
    let running = node.run(Vec::new());

    assert_at(running.account_proof(TRADER).await, &last);
    let Stopped { ended, entries, .. } = running.stop().await;
    ended.expect("the sequencer drains");
    assert_eq!(checkpoints(&entries).last().map(|checkpoint| checkpoint.seq), Some(last.seq), "no new checkpoint");
}

#[tokio::test]
async fn a_start_whose_tree_is_past_the_logs_last_checkpoint_answers_proofs_once_the_next_is_durable() {
    let mut node = Node::new("roots-read-past-checkpoint", None);
    node.seq.sequence(deposit(100, "roots-read-deposit")).expect("applies").expect("admitted");
    node.seq.flush().expect("the log flushes");
    // A root of the tip, which no checkpoint in the log holds, as a recovery that computes one has.
    node.seq.state_root();
    let paced = node.pace_next();
    let syncs = node.hold_syncs();
    let mut running = node.run(Vec::new());
    assert!(running.account_proof(TRADER).await.is_none(), "no checkpoint the log holds has the tree's root");

    let (reply, replied) = oneshot::channel();
    running.send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).await;
    syncs.started();
    assert!(running.account_proof(TRADER).await.is_none(), "the next checkpoint is not durable yet");
    syncs.open();
    timeout(WAIT, replied).await.expect("the tick is answered").expect("stamped");
    let checkpoint = running.checkpoint().await;
    assert_eq!(checkpoint.seq, paced);
    assert_at(running.account_proof(TRADER).await, &checkpoint);

    let Stopped { ended, entries, .. } = running.stop().await;
    ended.expect("the sequencer drains");
    replay_trusted(&entries).expect("every checkpoint's root is the replay's");
}

#[tokio::test]
async fn after_a_discarding_activation_proofs_wait_for_its_checkpoint_to_be_durable() {
    let mut node = Node::new("roots-read-discard", None);
    node.seq.sequence(deposit(100, "roots-read-deposit")).expect("applies").expect("admitted");
    node.seq.activate_protocol_at(ProtocolVersion::V26).expect("V26 activates");
    let mut node = node.restart();
    // The restart has no trusted time yet, so `V27`, which discards the tree, waits for the first sample.
    node.seq.activate_protocol_at(ProtocolVersion::V27).expect("queued");
    let sample = sample_around(node.clock.as_ref());
    let syncs = node.hold_syncs();
    let mut running = node.run(Vec::new());
    let before = running.account_proof(TRADER).await.expect("the tree is at the last checkpoint");

    running.send(Request::TimeSample(sample)).await;
    let (reply, synced) = oneshot::channel();
    running.send(Request::Sync(reply)).await;
    syncs.started();
    assert!(running.account_proof(TRADER).await.is_none(), "the discarded tree's versions are gone");
    syncs.open();
    let activated = timeout(WAIT, synced).await.expect("the sync is answered").expect("synced").expect("a seq");
    let checkpoint = running.checkpoint().await;
    assert_eq!(checkpoint.seq, activated, "the activation is checkpointed");
    assert!(checkpoint.seq > before.seq);
    assert_at(running.account_proof(TRADER).await, &checkpoint);

    let Stopped { ended, entries, .. } = running.stop().await;
    ended.expect("the sequencer drains");
    let replayed = replay_trusted(&entries).expect("every checkpoint's root is the replay's");
    assert!(replayed.activations.contains(&(ProtocolVersion::V27, activated)), "{:?}", replayed.activations);
}
