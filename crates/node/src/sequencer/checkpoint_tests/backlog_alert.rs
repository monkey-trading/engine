//! `checkpoint_backlog` fires when a hold outlasts the stall bound and resolves only when the poster
//! has caught up: a checkpoint signed around the hold (a handover's, the drain's) does not end it,
//! and an episode an earlier run left open ends once the chain has caught up.

use std::io::Write;
use std::time::Duration;

use super::hold_back::{Node, STALLED_AFTER, config};
use super::*;
use crate::alerts::{AlertLine, Router, Status, capture_until_dropped, pages_for};
use crate::checkpoints::{Registry, Work};
use crate::log::{Checkpoint, CheckpointParent, MANIFEST};
use crate::sequencer::Request;
use crate::sequencer::checkpoints::HandoverGate;
use crate::test_support::sample_around;

const SECOND: Duration = Duration::from_secs(1);

fn key() -> String {
    "checkpoint_backlog{}".to_owned()
}

#[test]
fn a_hold_is_a_backlog_only_past_the_stall_bound_and_a_final_checkpoint_restarts_the_bound() {
    let mut node = Node::new("backlog-bound");
    let mut alerts = Alerts::new();
    let first = node.tick_signs();
    // A finality delay inside the bound: nothing fires a second short of it.
    node.tick_holds();
    node.advance(STALLED_AFTER - SECOND);
    node.tick_holds();
    assert_eq!(alerts.pages(), []);

    // A checkpoint becoming final restarts the bound: the next hold is not counted from the last.
    node.seq.checkpoint_status(Some(first.seq), Some(Health::Healthy)).unwrap();
    let second = node.tick_signs();
    node.tick_holds();
    node.advance(STALLED_AFTER - SECOND);
    node.tick_holds();
    assert_eq!(alerts.pages(), [], "the bound counts from the hold after the last final checkpoint");

    // The bound reached with none final: one episode however many attempts it spans.
    node.advance(SECOND);
    for _ in 0..3 {
        node.tick_holds();
    }
    assert_eq!(alerts.pages(), [(Status::Firing, key())]);
    assert_eq!(alerts.fired(), 1);

    node.seq.checkpoint_status(Some(second.seq), Some(Health::Healthy)).unwrap();
    assert_eq!(alerts.pages(), [(Status::Firing, key()), (Status::Resolved, key())]);
    node.tick_signs();
    assert_eq!(alerts.pages().len(), 2);
}

#[test]
fn a_handovers_final_checkpoint_leaves_a_stall_open_until_one_becomes_final() {
    let (mut node, mut alerts, _) = stalled("backlog-handover");
    assert!(matches!(node.seq.handover_checkpoint().unwrap(), HandoverGate::Admitted));
    let forced = node.queued.try_recv().expect("the handover's checkpoint").checkpoint;
    assert_eq!(alerts.pages(), [(Status::Firing, key())], "signed around the hold, still not final");

    node.final_at(&forced);
    assert_eq!(alerts.pages(), [(Status::Firing, key()), (Status::Resolved, key())]);
}

#[tokio::test]
async fn a_drains_final_checkpoint_leaves_a_stall_open() {
    let (node, mut alerts, _) = stalled("backlog-drain");
    let Node { seq, mut queued, .. } = node;
    let last = seq.last_seq().unwrap();
    let (_, closed) = mpsc::channel::<Request>(1);
    seq.run(closed).await.unwrap();
    assert_eq!(queued.try_recv().unwrap().checkpoint.seq, last, "the drain signed its final checkpoint");
    assert_eq!(alerts.pages(), [(Status::Firing, key())], "a shutdown pages no resolve during a stall");
}

#[tokio::test]
async fn a_handover_refused_after_its_checkpoint_signed_leaves_a_stall_open_and_fires_once() {
    let (node, mut alerts, _) = stalled("backlog-failed-seal");
    let Node { seq, dir, mut queued, .. } = node;
    // The log's manifest changes under the writer, so sealing the tail fails after the checkpoint.
    let mut manifest = std::fs::OpenOptions::new().append(true).open(dir.join(MANIFEST)).unwrap();
    manifest.write_all(b"\n").unwrap();
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(seq.run(rx));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    tx.send(Request::FinalCheckpoint(stop)).await.unwrap();
    let err = stopped.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("sealing the log"), "{err:#}");
    queued.try_recv().expect("the checkpoint was signed before the seal failed");
    assert_eq!(alerts.pages(), [(Status::Firing, key())]);

    // Sequencing goes on and the next held attempt is the same episode.
    let (reply, replied) = tokio::sync::oneshot::channel();
    tx.send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).await.unwrap();
    replied.await.unwrap();
    assert!(!task.is_finished());
    assert_eq!(alerts.pages(), [(Status::Firing, key())]);
    assert_eq!(alerts.fired(), 1, "no second Firing while the stall goes on");
}

#[test]
fn an_episode_open_across_a_restart_ends_once_the_chain_has_caught_up() {
    let (node, mut alerts, unfinal) = stalled("backlog-restart-final");
    let anchor = node.anchor.clone();
    let (mut seq, mut queued) = restart(node, &anchor);
    assert_eq!(alerts.pages(), [(Status::Firing, key())]);

    // Still stalled after the restart: a checkpoint signed around the hold ends nothing.
    seq.sequence(Command::Tick).unwrap().unwrap();
    seq.checkpoint().unwrap();
    queued.try_recv().expect("the forced checkpoint");
    assert_eq!(alerts.pages(), [(Status::Firing, key())]);

    // The recovered checkpoint becoming final is the poster catching up.
    seq.checkpoint_status(Some(unfinal.seq), Some(Health::Healthy)).unwrap();
    assert_eq!(alerts.pages(), [(Status::Firing, key()), (Status::Resolved, key())]);
}

#[test]
fn an_episode_open_across_a_restart_ends_at_the_first_checkpoint_signed_with_nothing_on_its_way() {
    let (node, mut alerts, unfinal) = stalled("backlog-restart-signed");
    // The stalled checkpoint became final while the node was down.
    let (mut seq, mut queued) = restart(node, &unfinal);
    assert_eq!(alerts.pages(), [(Status::Firing, key())]);

    seq.sequence(Command::Tick).unwrap().unwrap();
    queued.try_recv().expect("nothing is on its way, so the cadence signs");
    assert_eq!(alerts.pages(), [(Status::Firing, key()), (Status::Resolved, key())]);
}

// ── Helpers ──

/// A node whose last signed checkpoint has stayed unfinal for the stall bound, with
/// `checkpoint_backlog` firing.
fn stalled(name: &str) -> (Node, Alerts, Checkpoint) {
    let mut node = Node::new(name);
    let mut alerts = Alerts::new();
    let unfinal = node.tick_signs();
    node.tick_holds();
    node.advance(STALLED_AFTER);
    node.tick_holds();
    assert_eq!(alerts.pages(), [(Status::Firing, key())]);
    (node, alerts, unfinal)
}

/// Restarts `node` from its log with `final_at_restart` the newest checkpoint the vault has final;
/// recovery finds the checkpoints after it unposted.
fn restart(node: Node, final_at_restart: &Checkpoint) -> (Sequencer, mpsc::Receiver<Work>) {
    let Node { mut seq, dir, clock, .. } = node;
    seq.flush().unwrap();
    drop(seq);
    let parent = CheckpointParent {
        has_checkpoint: true,
        seq: final_at_restart.seq,
        root: final_at_restart.root.parse().unwrap(),
    };
    let (sink, queued) = mpsc::channel(64);
    let (withdrawals, _signed) = mpsc::unbounded_channel();
    let cfg = crate::sequencer::SequencerConfig {
        checkpoint_parent: parent,
        ..config(&dir, &clock, sink, Registry::default(), None)
    };
    let mut seq =
        Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(16).0, withdrawals).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.checkpoint_status(Some(final_at_restart.seq), Some(Health::Healthy)).unwrap();
    (seq, queued)
}

/// The alert lines logged on this thread so far, and the pages the router would send for them.
struct Alerts {
    _guard: tracing::subscriber::DefaultGuard,
    router: Router,
    lines: Vec<AlertLine>,
}

impl Alerts {
    fn new() -> Self {
        let (_guard, router) = capture_until_dropped();
        Self { _guard, router, lines: Vec::new() }
    }

    fn pages(&mut self) -> Vec<(Status, String)> {
        self.lines.extend(self.router.take());
        pages_for(self.lines.clone())
    }

    /// How many Firing lines were logged, repeats included.
    fn fired(&mut self) -> usize {
        self.pages();
        self.lines.iter().filter(|line| line.severity.is_some()).count()
    }
}
