//! A follower's start from its replica: the copy is not read again.

use std::sync::Arc;

use engine_core::command::Command;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::*;
use crate::log::{Manifest, SegmentedLog};
use crate::sequencer::{Request, Sequencer};
use crate::tee::EnclaveKey;
use crate::test_support::{SEGMENT_COMMANDS, log_entries, sample_around, scratch_dir, sequencer_config};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// Once the replica is built, the sequencer starts from it and the copy's open writer: a host that
/// alters the copy's files after the build changes nothing the start reads, and the next command
/// follows the replica's last.
#[tokio::test]
async fn a_follower_starts_the_sequencer_from_its_replica_without_reading_the_copy_again() {
    let dir = scratch_dir("replica-start");
    crate::standby::tests::ticks(&dir, 10).await;
    let follower = Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, signer(), [signer()].into(), None, |_| {}).unwrap();
    let last = follower.replica().last_seq().expect("the copy holds commands");
    let now = follower.replica().engine().now();

    // The host alters a completed segment: a start reading the copy again would refuse it.
    let listed = Manifest::load(&dir).unwrap().segments[0].clone();
    let mut bytes = std::fs::read(dir.join(&listed.file)).unwrap();
    bytes[1] ^= 0x01;
    std::fs::write(dir.join(&listed.file), bytes).unwrap();
    assert!(SegmentedLog::open(&dir, |_| Ok(())).is_err(), "reading the altered copy again succeeds");

    let log = follower.into_log(None, signer(), None).unwrap();
    let (manifest, published) = watch::channel(Manifest::default());
    let (stream, _) = broadcast::channel(64);
    let (withdrawals, _custody) = mpsc::unbounded_channel();
    let key = Arc::new(EnclaveKey::test_key());
    let mut sequencer = Sequencer::recover_from(log, sequencer_config(&dir, manifest), key, stream, withdrawals)
        .expect("the sequencer starts from the replica");
    sequencer.trust(sample_around(&crate::clock::SystemClock)).unwrap();
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(sequencer.run(rx));
    let (reply, events) = oneshot::channel();
    let query = Box::new(move |engine: &engine_core::Engine| {
        let _ = reply.send((engine.last_seq(), engine.now()));
    });
    tx.send(Request::Query(query)).await.unwrap();
    assert_eq!(events.await.unwrap(), (Some(last), now), "the sequencer's engine is not the replica's");
    let (reply, applied) = oneshot::channel();
    tx.send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).await.unwrap();
    applied.await.unwrap();
    drop(tx);
    task.await.unwrap().unwrap();
    assert_eq!(published.borrow().flushed.map(|flushed| flushed.seq), Some(last + 1), "the writer published nothing");
    let appended = log_entries_after(&dir, &listed.file, last);
    assert!(
        appended.iter().any(|entry| matches!(entry, LogEntry::Command(command) if command.sequenced.seq == last + 1)),
        "the next command did not follow the replica's last: {appended:?}"
    );
}

/// A handover's tail extends the copy and its replica, entry by entry, after the copy's tip; the
/// entries the copy already holds past its last command repeat it and are not appended again.
#[tokio::test]
async fn a_tail_extends_the_copy_and_the_replica_after_the_tip() {
    let operator = scratch_dir("replica-tail-operator");
    crate::standby::tests::ticks(&operator, 6).await;
    let dir = scratch_dir("replica-tail-copy");
    crate::standby::tests::copy_log(&operator, &dir);
    crate::standby::tests::ticks(&operator, 5).await;
    let mut follower =
        Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, signer(), [signer()].into(), None, |_| {}).unwrap();
    let (tip, _) = follower.tip().unwrap();
    let held = log_entries(&dir).len();
    let tail = lines_after(&operator, tip);

    let taken = follower.take_tail(tip, &tail).unwrap();
    let expected = log_entries(&operator);
    assert_eq!(taken as usize, expected.len() - held, "the copy did not take exactly the entries it lacked");
    drop(follower);
    assert_eq!(log_entries(&dir), expected, "the copy is not the operator's log");

    // A tail whose entry the host altered does not extend the copy.
    let copy = scratch_dir("replica-tail-forged");
    crate::standby::tests::copy_log(&dir, &copy);
    crate::standby::tests::ticks(&operator, 2).await;
    let mut follower =
        Follower::open(&copy, SEGMENT_COMMANDS, DOMAIN, signer(), [signer()].into(), None, |_| {}).unwrap();
    let (tip, _) = follower.tip().unwrap();
    let forged = String::from_utf8(lines_after(&operator, tip)).unwrap().replacen("\"ts\":", "\"ts\":1", 1);
    let err = follower.take_tail(tip, forged.as_bytes()).expect_err("a forged entry extended the copy");
    assert!(crate::standby::rollback_of(&err).is_some(), "{err:#}");
}

// ── Helpers ──

/// The test sequencer's enclave, which seals and signs its log.
fn signer() -> AccountId {
    AccountId(EnclaveKey::test_key().address())
}

/// The JSON lines the log in `dir` holds after the command at `after`, as a predecessor sends them.
fn lines_after(dir: &std::path::Path, after: u64) -> Vec<u8> {
    let mut previous = None;
    let mut bytes = Vec::new();
    for entry in log_entries(dir) {
        let offset = offset_of(previous, &entry).unwrap();
        previous = Some(offset);
        if offset > Offset::start_of(after) {
            bytes.extend(serde_json::to_vec(&entry).unwrap());
            bytes.push(b'\n');
        }
    }
    bytes
}

/// The entries of the segments in `dir` named after `altered`, the segment the host altered, or
/// holding commands after `last`.
fn log_entries_after(dir: &std::path::Path, altered: &str, last: u64) -> Vec<LogEntry> {
    let mut entries = Vec::new();
    for file in std::fs::read_dir(dir).unwrap().map(Result::unwrap) {
        let name = file.file_name().to_string_lossy().into_owned();
        if name == altered || crate::log::segment_seq(&name).is_none_or(|first| first > last + 1) {
            continue;
        }
        let _ = crate::log::read_file(&file.path(), |entry| {
            entries.push(entry);
            Ok(())
        });
    }
    entries
}
