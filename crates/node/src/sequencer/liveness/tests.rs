//! The sequencer under the liveness rule: commands wait on its gate, a checkpoint waits for a
//! member's confirmation of its seq, and the hold it signs held heads under.

use std::sync::Arc;
use std::time::Duration;

use engine_core::command::Command;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::clock::test_clock::TestClock;
use crate::liveness::Gate;
use crate::log::{LogEntry, Manifest};
use crate::log_heads::{Head, Hold, Publisher};
use crate::sequencer::liveness::{DEFER_MAX, PACED_STALL};
use crate::sequencer::{Request, Sequencer, SequencerConfig};
use crate::tee::EnclaveKey;
use crate::test_support::{
    CHECKPOINT_EVERY, capture_alerts_and_resolutions, log_entries, sample_around, scratch_dir, sequencer_config,
};
use crate::trusted_time::{NO_SAMPLE_GRACE, Sample};

#[tokio::test]
async fn a_checkpoint_waits_for_a_member_to_confirm_its_seq_and_nothing_is_stamped_past_it() {
    let dir = scratch_dir("seq-liveness-checkpoint");
    let gate = Gate::closed();
    gate.fence(false);
    gate.checked(true);
    gate.bound_stamps(u64::MAX);
    // On a clock that stands still, so the paced checkpoint's wait never runs past its stall.
    let clock = Arc::new(TestClock::at(1_800_000_000_000));
    let (seq, _hold) = sequencer_at(&dir, gate.clone(), &clock);
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

/// A paced checkpoint no member confirms holds stamps for `PACED_STALL`, then is given up on with
/// nothing signed, and stamping goes on: a member moving to follow a new operator, or confirmations
/// lapsed at a boundary, cost no prices. Once paced checkpoints have been given up on for `DEFER_MAX`,
/// the next holds every stamp, under Warning `checkpoint_deferred`, until a member confirms its seq,
/// which writes it and resolves the alert.
#[tokio::test]
async fn a_paced_checkpoint_holds_stamps_for_its_stall_until_paced_ones_were_deferred_too_long() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let dir = scratch_dir("seq-liveness-paced-stall");
    let gate = Gate::closed();
    gate.fence(false);
    gate.checked(true);
    gate.bound_stamps(u64::MAX);
    let clock = Arc::new(TestClock::at(1_800_000_000_000));
    let (seq, _hold) = sequencer_at(&dir, gate.clone(), &clock);
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(seq.run(rx));
    let first = stamp_until_a_checkpoint_waits(&tx).await;
    clock.advance(PACED_STALL - Duration::from_millis(1));
    assert!(!submit(&tx).await, "the paced checkpoint gave up before its stall ran out");
    clock.advance(Duration::from_millis(1));
    assert!(submit(&tx).await, "the paced checkpoint held stamps past its stall");
    let signed = |dir| log_entries(dir).into_iter().filter(|entry| matches!(entry, LogEntry::Checkpoint(_))).count();
    assert_eq!(signed(&dir), 0, "a checkpoint was signed at seq {first} with no member's confirmation");
    assert!(alerts.try_recv().is_err(), "a paced checkpoint given up on within the deferral alerted");

    // Given up on for `DEFER_MAX`, the next paced checkpoint holds until a member confirms its seq.
    clock.advance(DEFER_MAX);
    let due = stamp_until_a_checkpoint_waits(&tx).await;
    clock.advance(PACED_STALL * 4);
    assert!(!submit(&tx).await, "a paced checkpoint deferred past the limit gave up again");
    let alert = alerts.try_recv().expect("no alert once paced checkpoints were deferred past the limit");
    assert_eq!(alert["alert"].as_str(), "checkpoint_deferred");
    gate.confirm(due);
    assert!(!submit(&tx).await, "the request that finds the confirmation is refused, then the checkpoint written");
    assert!(submit(&tx).await, "sequencing resumes");
    assert_eq!(
        resolved.try_recv().expect("the alert was not resolved")["alert_resolved"].as_str(),
        "checkpoint_deferred"
    );
    drop(tx);
    task.await.unwrap().unwrap();
    let at_due = log_entries(&dir).into_iter().any(|entry| matches!(entry, LogEntry::Checkpoint(c) if c.seq == due));
    assert!(at_due, "no checkpoint at the confirmed seq {due}");
}

/// A handover's final checkpoint that joins a paced one waiting at its seq is never given up on: past
/// the stall and the deferral limit nothing is stamped or signed, and once a member confirms its seq it
/// is written and the stop goes ahead.
#[tokio::test]
async fn a_final_checkpoint_joining_a_paced_wait_is_never_given_up() {
    let dir = scratch_dir("seq-liveness-joined-final");
    let gate = Gate::closed();
    gate.fence(false);
    gate.checked(true);
    gate.bound_stamps(u64::MAX);
    let clock = Arc::new(TestClock::at(1_800_000_000_000));
    let (seq, _hold) = sequencer_at(&dir, gate.clone(), &clock);
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(seq.run(rx));
    let due = stamp_until_a_checkpoint_waits(&tx).await;
    let (stop, stopped) = oneshot::channel();
    tx.send(Request::FinalCheckpoint(stop)).await.unwrap();
    clock.advance(DEFER_MAX + PACED_STALL);
    assert!(!submit(&tx).await, "a command was stamped past the final checkpoint's seq");
    let signed = log_entries(&dir).into_iter().any(|entry| matches!(entry, LogEntry::Checkpoint(c) if c.seq == due));
    assert!(!signed, "the final checkpoint was signed with no member's confirmation");
    gate.confirm(due);
    let _ = submit(&tx).await;
    stopped.await.unwrap().expect("the handover was refused though a member confirmed its seq");
    task.await.unwrap().unwrap();
    let signed = log_entries(&dir).into_iter().any(|entry| matches!(entry, LogEntry::Checkpoint(c) if c.seq == due));
    assert!(signed, "no final checkpoint at seq {due}");
}

/// A fenced enclave, or one whose start check has not passed, signs no checkpoint, however far a
/// member confirmed: a member may hold more than its log, or another history.
#[tokio::test]
async fn a_fenced_or_unchecked_enclave_signs_no_checkpoint_not_even_at_shutdown() {
    for (fenced, checked) in [(false, true), (true, true), (false, false)] {
        let dir = scratch_dir("seq-liveness-fenced-checkpoint");
        let gate = Gate::default();
        let (seq, _hold) = sequencer(&dir, gate.clone());
        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(seq.run(rx));
        assert!(submit(&tx).await && submit(&tx).await);
        let last = last_seq(&tx).await;
        gate.fence(fenced);
        gate.checked(checked);
        drop(tx);
        task.await.unwrap().unwrap();
        let signed = log_entries(&dir).iter().any(|entry| matches!(entry, LogEntry::Checkpoint(c) if c.seq == last));
        let case = format!("fenced {fenced}, checked {checked}");
        assert_eq!(signed, !fenced && checked, "{case}: the drain's final checkpoint at seq {last}");
    }
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

/// The hold the heads are signed under is the one trusted time shows, whatever stops the stamp: a halt
/// that starts while a checkpoint waits, while the gate is closed, or with stamps past the bound is
/// published as a halt, and its end, with the stamp still stopped, as live.
#[test]
fn a_halt_is_published_whatever_else_stops_the_stamp() {
    type Stopping = fn(&mut Sequencer, &Gate);
    let cases: [(&str, Stopping); 3] = [
        ("a checkpoint waits", |seq, _| seq.checkpoint_waits = Some(true)),
        ("the gate is closed", |_, gate| gate.lag(true)),
        ("the stamp is past the bound", |_, gate| gate.bound_stamps(0)),
    ];
    for (index, (case, stopping)) in cases.into_iter().enumerate() {
        let clock = Arc::new(TestClock::at(1_800_000_000_000));
        let gate = Gate::default();
        let (mut seq, hold) = sequencer_at(&scratch_dir(&format!("seq-liveness-halt-{index}")), gate.clone(), &clock);
        assert!(seq.admitted_ts(None).is_some(), "{case}: trusted time admits a stamp");
        stopping(&mut seq, &gate);
        clock.advance(NO_SAMPLE_GRACE + Duration::from_secs(1));
        assert_eq!(seq.admitted_ts(None), None, "{case}");
        assert_eq!(*hold.borrow(), Hold::TimeHalt, "{case}: the halt was not published");
        seq.trust(sample_around(clock.as_ref())).unwrap();
        assert_eq!(seq.admitted_ts(None), None, "{case}: still stopped");
        assert_eq!(*hold.borrow(), Hold::Live, "{case}: the halt's end was not published");
    }
}

/// A handover's stop that finds its final checkpoint waiting for a member's confirmation waits with
/// nothing stamped, and stops once a member confirms: a boundary handover under the rule completes
/// however far the confirmations trail the tip. With no confirmation it is refused once the wait runs
/// out, and sequencing goes on after a later one.
#[tokio::test]
async fn a_handover_waits_for_its_final_checkpoints_confirmation() {
    for confirmed in [true, false] {
        let dir = scratch_dir(&format!("seq-liveness-handover-{confirmed}"));
        let gate = Gate::default();
        gate.confirm(0);
        let (seq, _hold) = sequencer(&dir, gate.clone());
        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(seq.run(rx));
        assert!(submit(&tx).await && submit(&tx).await);
        let last = last_seq(&tx).await;
        let (stop, stopped) = oneshot::channel();
        tx.send(Request::FinalCheckpoint(stop)).await.unwrap();
        assert!(!submit(&tx).await, "a command was stamped while the final checkpoint waited");
        if confirmed {
            gate.confirm(last);
            // A request wakes the loop that finds the confirmation; nothing after the stop is stamped.
            let _ = submit(&tx).await;
            stopped.await.unwrap().expect("the handover was refused though a member confirmed its seq");
            task.await.unwrap().unwrap();
            let entries = log_entries(&dir);
            let signed = entries.iter().any(|entry| matches!(entry, LogEntry::Checkpoint(c) if c.seq == last));
            assert!(signed, "no final checkpoint at seq {last}");
        } else {
            tokio::time::sleep(Duration::from_millis(400)).await;
            let _ = submit(&tx).await;
            let refused = stopped.await.unwrap().expect_err("the handover stopped with no confirmation");
            assert!(format!("{refused:#}").contains("waited"), "{refused:#}");
            gate.confirm(u64::MAX);
            let _ = submit(&tx).await;
            assert!(submit(&tx).await, "sequencing did not go on after the refused handover");
            drop(tx);
            task.await.unwrap().unwrap();
        }
    }
}

// ── Helpers ──

/// A trusted sequencer over `dir` whose head publisher shares `gate`, with the hold it publishes.
fn sequencer(dir: &std::path::Path, gate: Gate) -> (Sequencer, watch::Receiver<Hold>) {
    let (manifest, _) = watch::channel(Manifest::default());
    let config = sequencer_config(dir, manifest);
    let clock = config.clock.clone();
    with_publisher(config, gate, sample_around(clock.as_ref()))
}

/// `sequencer`, on `clock`.
fn sequencer_at(dir: &std::path::Path, gate: Gate, clock: &Arc<TestClock>) -> (Sequencer, watch::Receiver<Hold>) {
    let (manifest, _) = watch::channel(Manifest::default());
    let config = SequencerConfig { clock: clock.clone(), ..sequencer_config(dir, manifest) };
    with_publisher(config, gate, sample_around(clock.as_ref()))
}

/// The sequencer `config` recovers, trusting `sample`, its head publisher sharing `gate`, with the hold
/// it publishes.
fn with_publisher(config: SequencerConfig, gate: Gate, sample: Sample) -> (Sequencer, watch::Receiver<Hold>) {
    let (stream, _) = broadcast::channel(16);
    let (withdrawals, _) = mpsc::unbounded_channel();
    let mut seq = Sequencer::recover(config, Arc::new(EnclaveKey::generate()), stream, withdrawals).unwrap();
    let mut publisher = Publisher::for_test(watch::channel(Head::default()).0, Default::default());
    publisher.gate = gate;
    let hold = publisher.hold.subscribe();
    seq.log_heads = Some(publisher);
    seq.trust(sample).unwrap();
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

/// Stamps ticks until a checkpoint waits, within `CHECKPOINT_EVERY` of the last, and returns its seq.
async fn stamp_until_a_checkpoint_waits(tx: &mpsc::Sender<Request>) -> u64 {
    let mut stamped = 0;
    while submit(tx).await {
        stamped += 1;
        assert!(stamped <= CHECKPOINT_EVERY, "a checkpoint is due within {CHECKPOINT_EVERY} commands");
    }
    last_seq(tx).await
}

/// Submits a tick and reports whether it was stamped.
async fn submit(tx: &mpsc::Sender<Request>) -> bool {
    let (reply, replied) = oneshot::channel();
    tx.send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).await.unwrap();
    replied.await.is_ok()
}

#[tokio::test]
async fn nothing_is_stamped_past_one_interval_after_the_newest_confirmation_whatever_the_reads_do() {
    let dir = scratch_dir("seq-liveness-stamp-bound");
    let gate = Gate::default();
    gate.bound_stamps(0);
    let (seq, _hold) = sequencer(&dir, gate.clone());
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(seq.run(rx));
    assert!(!submit(&tx).await, "a stamp past the bound is refused though the gate is otherwise open");
    gate.bound_stamps(u64::MAX);
    assert!(submit(&tx).await);
    drop(tx);
    task.await.unwrap().unwrap();
}
