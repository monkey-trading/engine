//! Batching: a batch ends after `MAX_BATCH` requests of any kind.

use super::*;
use tokio::sync::oneshot;

use crate::log::Manifest;
use crate::test_support::{dev_config, log_entries, scratch_dir, sequencer_config, trusted_now};

/// Reply-less commands keep the channel full, and a client's reply still goes out with the first
/// `MAX_BATCH` requests, while the rest wait for later batches.
#[tokio::test]
async fn a_reply_goes_out_at_the_batch_limit_while_reply_less_commands_keep_coming() {
    let seq = sequencer(&scratch_dir("seq-batch-flood"));
    let first = seq.next_seq;
    let (tx, rx) = mpsc::channel(3 * MAX_BATCH);
    let (reply, replied) = oneshot::channel();
    tx.try_send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).unwrap();
    let (at_end, seen) = oneshot::channel();
    tx.try_send(Request::Query(Box::new(move |e: &Engine| {
        let _ = at_end.send(e.last_seq());
    })))
    .unwrap();
    for _ in 0..2 * MAX_BATCH {
        tx.try_send(Request::Submit { cmd: Command::Tick, reply: None }).unwrap();
    }
    let task = tokio::spawn(seq.run(rx));
    replied.await.unwrap();
    // The query, in the reply's batch, sees it end after `MAX_BATCH` requests.
    assert_eq!(seen.await.unwrap(), Some(first + MAX_BATCH as u64 - 2));
    drop(tx);
    task.await.unwrap().unwrap();
}

/// A batch ends after `MAX_BATCH` requests whatever their kind: a query that is a batch's last
/// request runs before the command after it is applied.
#[tokio::test]
async fn a_batch_ends_after_max_batch_requests_of_any_kind() {
    for kind in ["submit without reply", "submit with reply", "time sample", "sync", "query"] {
        let dir = scratch_dir(&format!("seq-batch-{}", kind.replace(' ', "-")));
        let seq = sequencer(&dir);
        let (tx, rx) = mpsc::channel(MAX_BATCH + 1);
        for _ in 0..MAX_BATCH - 1 {
            let request = match kind {
                "submit without reply" => Request::Submit { cmd: Command::Tick, reply: None },
                "submit with reply" => Request::Submit { cmd: Command::Tick, reply: Some(oneshot::channel().0) },
                "time sample" => Request::TimeSample(trusted_now()),
                "sync" => Request::Sync(oneshot::channel().0),
                _ => Request::Query(Box::new(|_: &Engine| {})),
            };
            tx.try_send(request).unwrap();
        }
        let (at_end, seen) = oneshot::channel();
        tx.try_send(Request::Query(Box::new(move |e: &Engine| {
            let _ = at_end.send(e.last_seq());
        })))
        .unwrap();
        tx.try_send(Request::Submit { cmd: Command::Tick, reply: None }).unwrap();
        drop(tx);
        seq.run(rx).await.unwrap();
        let after = seen.await.unwrap().map_or(0, |s| s + 1);
        assert_eq!(replay(&log_entries(&dir)).unwrap().engine.last_seq(), Some(after), "{kind}");
    }
}

// ── Helpers ──

/// A sequencer on a fresh log in `dir`, with trusted time admitting stamps on the system clock.
fn sequencer(dir: &Path) -> Sequencer {
    let (stream, _) = broadcast::channel(16);
    let (withdrawals, _) = mpsc::unbounded_channel();
    let (manifest, _) = watch::channel(Manifest::default());
    let key = Arc::new(EnclaveKey::generate());
    let mut seq = Sequencer::recover(sequencer_config(dir, manifest), key, stream, withdrawals).unwrap();
    seq.trust(trusted_now()).unwrap();
    seq
}

/// A freeze — a detected log rollback — latches before its `Freeze` request dequeues: submissions
/// queued ahead of the request, but behind the detection, are refused like a refused stamp, and the
/// drain writes no final checkpoint over the last durable command.
#[tokio::test]
async fn a_freeze_refuses_commands_queued_before_the_freeze_request() {
    let dir = scratch_dir("seq-freeze");
    let freeze = crate::log_heads::FreezeLatch::default();
    let seq = {
        let (stream, _) = broadcast::channel(16);
        let (withdrawals, _) = mpsc::unbounded_channel();
        let (manifest, _) = watch::channel(Manifest::default());
        let key = Arc::new(EnclaveKey::generate());
        let mut seq = Sequencer::recover(sequencer_config(&dir, manifest), key, stream, withdrawals).unwrap();
        seq.log_heads = Some(crate::log_heads::Publisher::for_test(
            watch::channel(crate::log_heads::Head::default()).0,
            freeze.clone(),
        ));
        seq.trust(trusted_now()).unwrap();
        seq
    };
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(seq.run(rx));
    let (before_reply, before) = oneshot::channel();
    tx.send(Request::Submit { cmd: Command::Tick, reply: Some(before_reply) }).await.unwrap();
    before.await.expect("the submission queued before the detection is stamped");
    // The poster detects the rollback here: the latch trips before its `Freeze` request, so the
    // submission queued behind the detection is refused even though it sits ahead of the request.
    freeze.freeze();
    let (after_reply, after) = oneshot::channel();
    tx.send(Request::Submit { cmd: Command::Tick, reply: Some(after_reply) }).await.unwrap();
    tx.send(Request::Freeze).await.unwrap();
    drop(tx);
    task.await.unwrap().unwrap();
    assert!(after.await.is_err(), "the submission behind the detection is refused via the latch");
    let entries = log_entries(&dir);
    assert_eq!(
        entries.iter().filter(|entry| matches!(entry, LogEntry::Command(_))).count(),
        1,
        "exactly the command before the freeze is stamped"
    );
    assert!(
        !entries.iter().any(|entry| matches!(entry, LogEntry::Checkpoint(_))),
        "the drain writes no final checkpoint after a freeze"
    );
}

/// Every checkpoint path is a frozen no-op: the paced and explicit checkpoints, and a handover's
/// final checkpoint, write nothing once a rollback froze sequencing.
#[tokio::test]
async fn frozen_checkpoint_paths_write_nothing_and_refuse_the_handover() {
    let dir = scratch_dir("seq-freeze-checkpoints");
    let freeze = crate::log_heads::FreezeLatch::default();
    let seq = {
        let (stream, _) = broadcast::channel(16);
        let (withdrawals, _) = mpsc::unbounded_channel();
        let (manifest, _) = watch::channel(Manifest::default());
        let key = Arc::new(EnclaveKey::generate());
        let mut seq = Sequencer::recover(sequencer_config(&dir, manifest), key, stream, withdrawals).unwrap();
        seq.log_heads = Some(crate::log_heads::Publisher::for_test(
            watch::channel(crate::log_heads::Head::default()).0,
            freeze.clone(),
        ));
        seq.trust(trusted_now()).unwrap();
        seq
    };
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(seq.run(rx));
    let (stamped_reply, stamped) = oneshot::channel();
    tx.send(Request::Submit { cmd: Command::Tick, reply: Some(stamped_reply) }).await.unwrap();
    stamped.await.unwrap();
    freeze.freeze();
    tx.send(Request::Freeze).await.unwrap();
    let (stop, stopped) = oneshot::channel();
    tx.send(Request::FinalCheckpoint(stop)).await.unwrap();
    drop(tx);
    task.await.unwrap().unwrap();
    let error = stopped.await.unwrap().expect_err("a handover is refused while frozen");
    assert!(format!("{error:#}").contains("frozen"), "{error:#}");
    let entries = log_entries(&dir);
    assert_eq!(
        entries.iter().filter(|entry| matches!(entry, LogEntry::Command(_))).count(),
        1,
        "the command before the freeze is stamped"
    );
    assert!(
        !entries.iter().any(|entry| matches!(entry, LogEntry::Checkpoint(_))),
        "no checkpoint path writes after a freeze: scheduled, explicit, or a handover's final one"
    );
}

/// The frozen latch turns every checkpoint entry into a logged no-op and refuses stamps: the
/// paced and explicit checkpoints, and one a command's own schedule would write, sign nothing.
#[test]
fn frozen_checkpoint_entries_and_stamps_are_refused() {
    let dir = scratch_dir("seq-freeze-entries");
    let freeze = crate::log_heads::FreezeLatch::default();
    let seq = {
        let (stream, _) = broadcast::channel(16);
        let (withdrawals, _) = mpsc::unbounded_channel();
        let (manifest, _) = watch::channel(Manifest::default());
        let key = Arc::new(EnclaveKey::generate());
        let mut seq = Sequencer::recover(sequencer_config(&dir, manifest), key, stream, withdrawals).unwrap();
        seq.log_heads = Some(crate::log_heads::Publisher::for_test(
            watch::channel(crate::log_heads::Head::default()).0,
            freeze.clone(),
        ));
        seq.trust(trusted_now()).unwrap();
        seq
    };
    let mut seq = seq;
    assert!(seq.sequence(Command::Tick).unwrap().is_some(), "the command before the freeze stamps");
    seq.flush().unwrap();
    let entries = || log_entries(&dir);
    let before = entries().len();
    freeze.freeze();
    assert!(
        seq.sequence(Command::Tick).unwrap().is_none(),
        "a stamp after the detection is refused, like a refused clock"
    );
    seq.paced_checkpoint().unwrap();
    seq.checkpoint().unwrap();
    assert!(
        matches!(seq.handover_checkpoint().unwrap(), crate::sequencer::HandoverGate::Refused(_)),
        "a handover final checkpoint is refused while frozen"
    );
    seq.flush().unwrap();
    assert_eq!(
        entries().len(),
        before,
        "no checkpoint entry — scheduled, explicit, or handover — is written over frozen books"
    );
}

/// The latch is checked at the per-command stamping boundary, not only when a request is admitted:
/// a freeze between two catch-up stamps aborts the request — neither the remaining ticks nor its
/// own command are logged.
#[test]
fn a_freeze_mid_catch_up_refuses_the_remaining_ticks_and_the_command() {
    const LOG_START: i64 = 1_792_504_800_000;
    const DAY: i64 = 86_400_000;
    let dir = scratch_dir("seq-freeze-catch-up");
    let dev = dev_config();
    let genesis = &dev.genesis;
    let key = Arc::new(EnclaveKey::generate());
    let boot = |dir: &Path, at: i64| {
        let (stream, _) = broadcast::channel(16);
        let (withdrawals, _) = mpsc::unbounded_channel();
        let (manifest, _) = watch::channel(Manifest::default());
        let key = key.clone();
        let clock = Arc::new(crate::clock::test_clock::TestClock::at(at));
        let mut seq = Sequencer::recover(
            SequencerConfig { clock: clock.clone(), ..sequencer_config(dir, manifest) },
            key,
            stream,
            withdrawals,
        )
        .unwrap();
        seq.carry_toxicity_notional(genesis.params.toxicity.notional.expect("the dev config weighs by notional"))
            .unwrap();
        seq.activate_protocol().unwrap();
        if seq.needs_genesis() {
            seq.genesis(genesis).unwrap();
        }
        seq.trust(crate::test_support::sample_around(clock.as_ref())).unwrap();
        seq
    };
    let mut seq = boot(&dir, LOG_START);
    assert!(seq.sequence(crate::test_support::deposit(100, "dep-0")).unwrap().is_some());
    seq.flush().unwrap();
    drop(seq);

    // Restart after an outage of more than one catch-up batch of daily rollovers: the next
    // command owes several catch-up ticks.
    let after = LOG_START + (engine_core::corporate::MAX_CATCH_UP_BOUNDARIES as i64 + 40) * DAY;
    let freeze = crate::log_heads::FreezeLatch::default();
    let mut seq = boot(&dir, after);
    seq.log_heads = Some(crate::log_heads::Publisher::for_test(
        watch::channel(crate::log_heads::Head::default()).0,
        freeze.clone(),
    ));
    let ts = seq.admitted_ts(None).expect("trusted time admits a stamp");
    assert!(seq.engine.catch_up_pending(ts), "the outage owes catch-up ticks");
    let tick_at = if seq.last_ts < ts { ts - 1 } else { ts };
    // The drain's first tick, as the admitted request would have stamped it: everything up to here
    // — genesis, the pre-freeze deposit, the first tick — is durable and stands.
    seq.stamp_once(tick_at, Command::Tick).unwrap();
    seq.flush().unwrap();
    let before = log_entries(&dir).len();
    // The poster detects the rollback while the request is mid-catch-up.
    freeze.freeze();
    let events = seq.stamp(ts, crate::test_support::deposit(50, "dep-1")).unwrap();
    assert!(events.is_none(), "a request interrupted by the freeze refuses, nothing further stamped");
    // The common per-command boundary refuses directly, too: a tick stamped through it while
    // frozen logs nothing.
    assert!(seq.stamp_once(tick_at, Command::Tick).unwrap().is_empty(), "stamp_once refuses while frozen");
    seq.flush().unwrap();
    assert_eq!(
        log_entries(&dir).len(),
        before,
        "no later catch-up tick and no command the frozen request carried are logged"
    );
}
