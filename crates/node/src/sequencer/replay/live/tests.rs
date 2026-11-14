//! A live replica against a full replay of the same log, and its finish against recovery's.

use super::*;

use engine_core::command::Command;
use tokio::sync::watch;

use crate::log::{Manifest, PrefixRing, SegmentedLog};
use crate::sequencer::Sequencer;
use crate::tee::EnclaveKey;
use crate::test_support::{SEGMENT_COMMANDS, TestSequencer, log_entries, scratch_dir};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// Fed a log entry by entry, a replica holds what a full replay of the log gives: the state root,
/// the cumulative prefix after every command, and the last checkpoint.
#[tokio::test]
async fn a_replica_fed_entry_by_entry_equals_a_full_replay() {
    let dir = scratch_dir("live-replica-equals-replay");
    sequence_ticks(&dir, 20).await;
    let mut replayed = crate::sequencer::replay_path_with_attested(&dir, Some(DOMAIN), [signer()].into()).unwrap();
    let mut replica = LiveReplay::new(DOMAIN, [signer()].into());
    for entry in log_entries(&dir) {
        replica.apply(&entry).unwrap();
    }

    assert_eq!(replica.state_root(), replayed.engine.state_root());
    assert_eq!(replica.last_seq(), replayed.engine.last_seq());
    assert_eq!(replica.head_link(), B256::from(replayed.command_prefix));
    assert_eq!(replica.last_checkpoint(), replayed.last_signed_checkpoint.as_ref());
    for seq in 0..=replayed.engine.last_seq().unwrap() {
        let held = replica.link_at(seq).map(|held| held.link);
        assert!(held.is_some(), "the replica retains no link at seq {seq}");
        assert_eq!(held, replayed.prefix_ring.prefix_at(seq), "the links differ at seq {seq}");
    }
}

/// Finished at a checkpoint the vault finalized, the replica's recovery facts are those a recovery
/// replay anchored there gives: the anchor's work and stamp, the base, and the authority pinned to
/// the signer, so the sequencer validates it as it would the log read from disk.
#[tokio::test]
async fn a_replica_finishes_as_recovery_anchored_at_the_finalized_checkpoint() {
    let dir = scratch_dir("live-replica-anchor");
    sequence_ticks(&dir, 12).await;
    let anchor = log_entries(&dir)
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .nth(1)
        .expect("a second checkpoint");
    let parent = CheckpointParent { has_checkpoint: true, seq: anchor.seq, root: anchor.root.parse().unwrap() };
    let expected =
        Sequencer::read_log_for_recovery(&dir, Some(DOMAIN), Some(signer()), Some(parent), |_| {}).unwrap().replayed;
    let actual = live(&dir).into_log(writer(&dir), Some(parent), signer(), None).unwrap().replayed;

    assert_eq!(json(&actual.recovery_anchor_work), json(&expected.recovery_anchor_work));
    assert!(actual.recovery_anchor_work.is_some(), "the anchor's work was not captured");
    assert_eq!(json(&actual.recovery_work), json(&expected.recovery_work));
    assert_eq!(json(&actual.recovery_base), json(&expected.recovery_base));
    assert_eq!(actual.anchor_ts, expected.anchor_ts);
    actual.origin.require_recovery(DOMAIN, signer()).unwrap();
}

/// A replica that no longer retains the vault's finalized checkpoint does not start: the node
/// recovers from the log on disk instead.
#[tokio::test]
async fn a_replica_refuses_an_anchor_it_does_not_retain() {
    let dir = scratch_dir("live-replica-unretained");
    sequence_ticks(&dir, 6).await;
    let parent = CheckpointParent { has_checkpoint: true, seq: 1, root: B256::repeat_byte(0xab) };
    let err = live(&dir).into_log(writer(&dir), Some(parent), signer(), None).err().expect("an unretained anchor");
    assert!(format!("{err:#}").contains("is not among the checkpoints the replica retains"), "{err:#}");
}

/// The replica's authority is the signer of the checkpoint it ends at: another key does not start
/// from it.
#[tokio::test]
async fn a_replica_starts_only_under_the_key_that_signed_its_last_checkpoint() {
    let dir = scratch_dir("live-replica-other-signer");
    sequence_ticks(&dir, 6).await;
    let other = AccountId(EnclaveKey::generate().address());
    let err = live(&dir).into_log(writer(&dir), None, other, None).err().expect("another key started");
    assert!(format!("{err:#}").contains("does not end at a checkpoint"), "{err:#}");
}

/// A restart's replay and a replica's start each seed the sequencer's tip ring and the log-head
/// poster with the prefix after every one of the latest `PrefixRing::CAPACITY` commands, which the
/// sequencer's ring keeps whole: a tip or a posted head that far back is still evidence.
#[tokio::test]
async fn a_restart_and_a_replicas_start_retain_every_prefix_the_sequencer_retains() {
    let dir = scratch_dir("live-replica-ring");
    sequence_ticks(&dir, PrefixRing::CAPACITY + 16).await;
    let replayed = crate::sequencer::replay_path_with_attested(&dir, Some(DOMAIN), [signer()].into()).unwrap();
    let oldest = replayed.engine.last_seq().unwrap() + 1 - PrefixRing::CAPACITY as u64;
    let started = live(&dir).into_log(writer(&dir), None, signer(), None).unwrap().prefix_ring();
    for (ring, start) in [(&replayed.prefix_ring, "the restart"), (&started, "the replica's start")] {
        let seeded = crate::sequencer::prefixes::retained(ring);
        assert!(seeded.prefix_at(oldest).is_some(), "{start} seeded no prefix at seq {oldest}");
        assert_eq!(seeded.prefix_at(oldest), replayed.prefix_ring.prefix_at(oldest), "{start}'s prefix differs");
        assert_eq!(seeded.prefix_at(oldest - 1), None, "{start} retained past the capacity");
    }
}

// ── Helpers ──

/// `value` as JSON, to compare recovery facts that implement no equality.
fn json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

/// The test sequencer's enclave, which seals and signs its log.
fn signer() -> AccountId {
    AccountId(EnclaveKey::test_key().address())
}

/// Runs the test sequencer on `dir` for `count` ticks and stops it, which ends the log at a
/// checkpoint.
async fn sequence_ticks(dir: &std::path::Path, count: usize) {
    let node = TestSequencer::start(dir, 1024);
    for _ in 0..count {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
}

/// A replica of the log in `dir`.
fn live(dir: &std::path::Path) -> LiveReplay {
    let mut replica = LiveReplay::new(DOMAIN, [signer()].into());
    for entry in log_entries(dir) {
        replica.apply(&entry).unwrap();
    }
    replica
}

/// A writer at the end of the log in `dir`.
fn writer(dir: &std::path::Path) -> LogWriter {
    let (manifest, _) = watch::channel(Manifest::default());
    SegmentedLog::open(dir, |_| Ok(())).unwrap().into_writer(SEGMENT_COMMANDS, manifest).unwrap()
}
