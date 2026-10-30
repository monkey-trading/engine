//! A handover's stop is the checkpoint gate's call: admitted with the final checkpoint written,
//! refused with its causes while it would withhold one, and only an admitted stop ends the
//! sequencer's loop.

use super::*;
use crate::checkpoints::Work;
use crate::sequencer::Request;
use crate::sequencer::StreamBody;
use crate::sequencer::StreamMsg;
use crate::sequencer::checkpoints::HandoverGate;
use tokio::sync::oneshot;

#[test]
fn the_handover_checkpoint_is_the_gates_call_and_names_its_causes() {
    // Nothing sequenced cannot hand over a checkpoint.
    let dir = scratch_dir("handover-no-command");
    let (work, _queued) = mpsc::channel(8);
    let cfg = crate::sequencer::SequencerConfig {
        checkpoints: Some(work),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut fresh =
        Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(8).0, mpsc::unbounded_channel().0)
            .unwrap();
    let reason = refused(fresh.handover_checkpoint().unwrap(), "an empty log");
    assert!(format!("{reason:#}").contains("no command has applied"), "{reason:#}");

    let (dir, mut seq, mut queued) = gated("handover-gate", broadcast::channel(8).0);
    // Admitted: a checkpoint of the last command exists to hand over, written now or by the cadence.
    assert!(matches!(seq.handover_checkpoint().unwrap(), HandoverGate::Admitted), "a healthy gate admits");
    let last = seq.last_seq().unwrap();
    assert!(
        log_entries(&dir).iter().any(|entry| matches!(entry, LogEntry::Checkpoint(cp) if cp.seq == last)),
        "a checkpoint of the last command exists to hand over"
    );

    // A diverged-collateral verdict refuses with the cause and signs nothing: the last command's
    // own checkpoint attempt is already withheld.
    seq.checkpoint_status(None, Some(Health::Diverged)).unwrap();
    seq.sequence(Command::Tick).unwrap();
    let last = seq.last_seq().unwrap();
    let reason = refused(seq.handover_checkpoint().unwrap(), "diverged collateral");
    assert!(format!("{reason:#}").contains("collateral"), "{reason:#}");
    assert!(format!("{reason:#}").contains("withheld"), "{reason:#}");
    assert!(queued.try_recv().is_err(), "the withheld final checkpoint is not queued");
    assert!(
        !log_entries(&dir).iter().any(|entry| matches!(entry, LogEntry::Checkpoint(cp) if cp.seq == last)),
        "no checkpoint is signed at the last seq while collateral is diverged"
    );

    // The gate's recovery admits the stop, writing the last command's checkpoint.
    seq.checkpoint_status(None, Some(Health::Healthy)).unwrap();
    assert!(matches!(seq.handover_checkpoint().unwrap(), HandoverGate::Admitted), "a recovered gate admits");
    assert_eq!(queued.try_recv().unwrap().checkpoint.seq, seq.last_seq().unwrap());
}

#[tokio::test]
async fn an_admitted_handover_stop_ends_the_sequencer_at_its_final_checkpoint() {
    let (stream, mut seen) = broadcast::channel(16);
    let (dir, seq, _queued) = gated("handover-stop-admitted", stream);
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(seq.run(rx));

    // The stop is the last word: what sequences before it is covered, what follows never applies.
    tx.send(Request::Submit { cmd: Command::Tick, reply: None }).await.unwrap();
    let (stop, stopped) = oneshot::channel();
    tx.send(Request::FinalCheckpoint(stop)).await.unwrap();
    let (after, replied) = oneshot::channel();
    tx.send(Request::Submit { cmd: deposit(5, "after-the-stop"), reply: Some(after) }).await.unwrap();
    stopped.await.unwrap().unwrap();
    task.await.unwrap().unwrap();

    assert!(replied.await.is_err(), "the command after the stop was answered");
    let entries = log_entries(&dir);
    assert!(
        !entries.iter().any(|entry| {
            matches!(entry, LogEntry::Command(command) if serde_json::to_string(command).unwrap().contains("after-the-stop"))
        }),
        "the command after the stop sequenced"
    );
    let last = entries
        .iter()
        .rfind(|entry| matches!(entry, LogEntry::Command(_)))
        .and_then(|entry| match entry {
            LogEntry::Command(command) => Some(command.sequenced.seq),
            _ => None,
        })
        .unwrap();
    let checkpoint = entries
        .iter()
        .rfind(|entry| matches!(entry, LogEntry::Checkpoint(_)))
        .and_then(|entry| match entry {
            LogEntry::Checkpoint(cp) => Some(cp.seq),
            _ => None,
        })
        .unwrap();
    assert_eq!(checkpoint, last, "the final checkpoint covers the last command before the stop");
    let mut newest = None;
    loop {
        match seen.try_recv() {
            Ok(msg) => newest = Some(msg),
            Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(broadcast::error::TryRecvError::Closed | broadcast::error::TryRecvError::Empty) => break,
        }
    }
    assert!(
        matches!(newest.as_deref(), Some(StreamMsg { body: StreamBody::Checkpoint(cp), .. }) if cp.seq == last),
        "the stream's newest entry is the final checkpoint: {newest:?}"
    );
}

#[tokio::test]
async fn a_refused_handover_stop_goes_on_sequencing_until_the_gate_admits_it() {
    let (_dir, seq, _queued) = gated("handover-stop-refused", broadcast::channel(8).0);
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(seq.run(rx));

    tx.send(Request::CheckpointStatus { posted: None, health: Some(Health::Diverged), lifecycle: None }).await.unwrap();
    tx.send(Request::Submit { cmd: Command::Tick, reply: None }).await.unwrap();
    let (stop, stopped) = oneshot::channel();
    tx.send(Request::FinalCheckpoint(stop)).await.unwrap();
    let err = stopped.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("withheld"), "{err:#}");
    assert!(!task.is_finished(), "the refused stop ended the sequencer");

    // Sequencing goes on: a later command applies and is answered.
    let (reply, replied) = oneshot::channel();
    tx.send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).await.unwrap();
    replied.await.unwrap();

    // Once the gate admits the final checkpoint, the stop commits.
    tx.send(Request::CheckpointStatus { posted: None, health: Some(Health::Healthy), lifecycle: None }).await.unwrap();
    let (stop, stopped) = oneshot::channel();
    tx.send(Request::FinalCheckpoint(stop)).await.unwrap();
    stopped.await.unwrap().unwrap();
    task.await.unwrap().unwrap();
}

// ── Helpers ──

fn refused(gate: HandoverGate, what: &str) -> anyhow::Error {
    match gate {
        HandoverGate::Refused(reason) => reason,
        HandoverGate::Admitted => panic!("{what}: the stop was admitted"),
    }
}

/// A gated sequencer that checkpoints every command, after its bootstrap, on the log in `dir`.
fn gated(
    name: &str,
    stream: broadcast::Sender<Arc<StreamMsg>>,
) -> (std::path::PathBuf, Sequencer, mpsc::Receiver<Work>) {
    let dir = scratch_dir(name);
    let (sink, mut queued) = mpsc::channel(128);
    let mut genesis = dev_config().genesis;
    genesis.house_capital = Fixed::ZERO;
    let cfg = crate::sequencer::SequencerConfig {
        checkpoints: Some(sink),
        checkpoint_every: 1,
        genesis: crate::sequencer::genesis_commands(&genesis),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq =
        Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), stream, mpsc::unbounded_channel().0).unwrap();
    seq.activate_protocol_at(engine_core::protocol::ProtocolVersion::V23).unwrap();
    seq.genesis(&genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    while queued.try_recv().is_ok() {}
    (dir, seq, queued)
}
