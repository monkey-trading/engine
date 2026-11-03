//! With the log's syncs on their own thread, a signed checkpoint reaches the poster, and its
//! approvals the nonce-use queries, only once the batch holding it is durable. The sync is a gate the
//! test opens.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use engine_core::command::Command;
use tokio::sync::mpsc::{self, Receiver};
use tokio::sync::{broadcast, watch};

use crate::checkpoints::{ApprovalRegistry, Health, Registry, Work};
use crate::clock::test_clock::TestClock;
use crate::log::Manifest;
use crate::sequencer::pipeline::Batch;
use crate::sequencer::{Sequencer, SequencerConfig};
use crate::tee::EnclaveKey;
use crate::test_support::{
    deposit, dev_config, empty_custody_frontier, sample_around, scratch_dir, sequencer_config, withdraw,
};

/// Whether the held sync succeeds when the test lets it.
enum Verdict {
    Pass,
    Fail,
}

/// A gated sequencer whose bootstrap checkpoints are final, its syncs held for the test.
struct Gated {
    seq: Sequencer,
    queued: Receiver<Work>,
    approvals: ApprovalRegistry,
    permits: Sender<Verdict>,
}

impl Gated {
    fn new(name: &str, queue: usize) -> Self {
        let dir = scratch_dir(name);
        let clock = Arc::new(TestClock::default());
        let (sink, mut queued) = mpsc::channel(queue);
        let (withdrawals, _custody) = mpsc::unbounded_channel();
        let approvals = ApprovalRegistry::default();
        let cfg = SequencerConfig {
            clock: clock.clone(),
            checkpoints: Some(sink),
            checkpoint_every: u64::MAX,
            segment_commands: u64::MAX,
            checkpoint_registry: Some(Registry::default()),
            approval_registry: Some(approvals.clone()),
            ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
        };
        let mut seq =
            Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(64).0, withdrawals).unwrap();
        seq.activate_legacy_protocol().unwrap();
        seq.genesis(&dev_config().genesis).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        seq.sequence(empty_custody_frontier()).unwrap();
        seq.checkpoint().unwrap();
        seq.checkpoint_status(seq.last_seq(), Some(Health::Healthy)).unwrap();
        seq.sequence(deposit(1_000, "publish-funds")).unwrap().unwrap();
        seq.checkpoint().unwrap();
        seq.flush().unwrap();
        let anchor = std::iter::from_fn(|| queued.try_recv().ok()).last().expect("the funding checkpoint");
        seq.checkpoint_status(Some(anchor.checkpoint.seq), Some(Health::Healthy)).unwrap();
        let (permits, verdicts) = channel::<Verdict>();
        let verdicts = Mutex::new(verdicts);
        seq.pipeline_with_syncs(move |job| match verdicts.lock().unwrap().recv() {
            Ok(Verdict::Pass) => job.sync(),
            _ => anyhow::bail!("the disk failed"),
        })
        .unwrap();
        Self { seq, queued, approvals, permits }
    }

    /// Takes the batch so far and queues it behind the held sync, as `run` does.
    fn submit(&mut self) {
        let release = self.seq.stage(Batch::default()).unwrap();
        self.seq.submit(release).unwrap();
    }
}

#[test]
fn a_checkpoint_and_its_approvals_wait_for_the_sync_of_their_batch() {
    let mut node = Gated::new("publish-held", 8);
    node.seq.sequence(withdraw(1)).unwrap().unwrap();
    node.submit();
    assert!(node.queued.try_recv().is_err(), "the checkpoint is signed but not posted while its sync is held");
    assert!(node.approvals.lock().unwrap().is_empty(), "and its approval is not yet visible to nonce-use queries");
    node.permits.send(Verdict::Pass).unwrap();
    node.seq.settle().unwrap();
    let posted = node.queued.try_recv().expect("posted once the batch is durable");
    assert_eq!(Some(posted.checkpoint.seq), node.seq.last_seq());
    assert_eq!(node.approvals.lock().unwrap().len(), 1, "the approval it covers is registered with it");
}

#[test]
fn a_failed_sync_posts_and_registers_nothing() {
    let mut node = Gated::new("publish-failed", 8);
    node.seq.sequence(withdraw(1)).unwrap().unwrap();
    node.submit();
    node.permits.send(Verdict::Fail).unwrap();
    let stopped = node.seq.settle().expect_err("a failed sync stops the sequencer");
    assert!(format!("{stopped:#}").contains("syncing the log"), "{stopped:#}");
    assert!(node.queued.try_recv().is_err());
    assert!(node.approvals.lock().unwrap().is_empty());
}

#[test]
fn a_post_owed_a_slot_defers_the_next_checkpoint_instead_of_failing_the_post() {
    // One slot in the poster's queue, owed to the checkpoint waiting on its sync.
    let mut node = Gated::new("publish-capacity", 1);
    node.seq.sequence(withdraw(1)).unwrap().unwrap();
    node.submit();
    node.seq.sequence(Command::Tick).unwrap().unwrap();
    let signed = node.seq.last_checkpoint;
    node.seq.checkpoint().unwrap();
    assert_eq!(node.seq.last_checkpoint, signed, "the queue's only slot is spoken for, so no second one is signed");
    assert!(node.seq.gate.as_ref().unwrap().queue_full);
    node.permits.send(Verdict::Pass).unwrap();
    node.seq.settle().expect("the first post finds its slot");
    assert!(node.queued.try_recv().is_ok());
}
