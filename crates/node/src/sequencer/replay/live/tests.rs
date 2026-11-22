//! A live replica against a full replay of the same log, and its finish against recovery's.

use super::*;

use alloy::primitives::U256;

use engine_core::command::Command;
use tokio::sync::watch;

use crate::log::{Manifest, PrefixRing, SegmentedLog};
use crate::sequencer::Sequencer;
use crate::tee::EnclaveKey;
use crate::test_support::{SEGMENT_COMMANDS, TestSequencer, keep_time_fresh, log_entries, scratch_dir};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// Fed a log entry by entry, a replica holds what a full replay of the log gives: the state root,
/// the cumulative prefix after every command, and the last checkpoint.
#[tokio::test]
async fn a_replica_fed_entry_by_entry_equals_a_full_replay() {
    let dir = scratch_dir("live-replica-equals-replay");
    sequence_ticks(&dir, 20).await;
    let mut replayed =
        crate::sequencer::replay_path_with_attested(&dir, Some(DOMAIN), [signer()].into(), Default::default()).unwrap();
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
    let replayed =
        crate::sequencer::replay_path_with_attested(&dir, Some(DOMAIN), [signer()].into(), Default::default()).unwrap();
    let oldest = replayed.engine.last_seq().unwrap() + 1 - PrefixRing::CAPACITY as u64;
    let started = live(&dir).into_log(writer(&dir), None, signer(), None).unwrap().prefix_ring();
    for (ring, start) in [(&replayed.prefix_ring, "the restart"), (&started, "the replica's start")] {
        let seeded = crate::sequencer::prefixes::retained(ring);
        assert!(seeded.prefix_at(oldest).is_some(), "{start} seeded no prefix at seq {oldest}");
        assert_eq!(seeded.prefix_at(oldest), replayed.prefix_ring.prefix_at(oldest), "{start}'s prefix differs");
        assert_eq!(seeded.prefix_at(oldest - 1), None, "{start} retained past the capacity");
    }
}

/// A replica asked to keep a seq holds its command link and checkpoint once the retention windows have
/// passed them, as a promoted member's restart checks its promotion's fixed head and checkpoint;
/// without the request the link is gone.
#[tokio::test]
async fn a_kept_seq_outlives_the_retained_links() {
    let dir = scratch_dir("live-replica-kept");
    sequence_ticks(&dir, 40).await;
    let entries = log_entries(&dir);
    let checkpoint = entries
        .iter()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint.seq),
            _ => None,
        })
        .expect("an early checkpoint");
    let (mut kept, mut plain, mut early) = (live_empty(), live_empty(), live_empty());
    kept.keep(3);
    kept.keep(checkpoint);
    for entry in &entries {
        kept.apply(entry).unwrap();
        plain.apply(entry).unwrap();
        if early.last_seq() < Some(3) {
            early.apply(entry).unwrap();
        }
    }
    // The windows move on past seq 3 and the checkpoint, as a long log's replay leaves them.
    plain.forget_retained();
    kept.forget_retained();
    assert_eq!(plain.link_at(3), None, "seq 3 is past the retained links");
    assert_eq!(kept.link_at(3).map(|held| held.link), Some(early.head_link()), "the kept link is seq 3's");
    assert!(kept.checkpoint_at(checkpoint).is_some(), "the kept checkpoint");
}

// ── Helpers ──

/// An empty replica of the test sequencer's log.
fn live_empty() -> LiveReplay {
    LiveReplay::new(DOMAIN, [signer()].into())
}

/// A replica built before the vault named a key refuses that key's seals and checkpoints until it
/// admits the key, as a follower does once the vault's latest signer is a promotion's standby key.
#[tokio::test]
async fn a_replica_takes_a_key_the_vault_named_after_its_build_only_once_it_admits_it() {
    let dir = scratch_dir("live-replica-admit");
    sequence_ticks(&dir, 6).await;
    let other = AccountId(EnclaveKey::generate().address());
    let fed = |admitted: bool| {
        let mut replica = LiveReplay::new(DOMAIN, [other].into());
        if admitted {
            replica.admit(signer());
        }
        log_entries(&dir).iter().try_for_each(|entry| replica.apply(entry))
    };
    let refused = fed(false).unwrap_err();
    assert!(format!("{refused:#}").contains("verified attestation"), "{refused:#}");
    fed(true).unwrap();
}

/// A promoted start retires what the outgoing key signed past the vault's latest checkpoint: the
/// anchor's work stays for reconciliation, no outstanding checkpoint of another key is resumed, and the
/// next checkpoint extends the anchor; a restart resumes the promoted key's own outstanding ones, whose
/// chain starts at the anchor and would break any other start's.
#[test]
fn a_promoted_anchor_keeps_only_the_promoted_keys_outstanding_checkpoints() {
    let (outgoing, promoted) = (signer(), AccountId(EnclaveKey::generate().address()));
    let parent = |seq: u64| CheckpointParent { has_checkpoint: true, seq, root: B256::from(U256::from(seq)) };
    let work = |seq: u64, signer: AccountId, after: u64| crate::checkpoints::Work {
        checkpoint: crate::log::Checkpoint {
            parent: Some(parent(after)),
            seq,
            root: format!("{:#x}", B256::from(U256::from(seq))),
            signature: String::new(),
            signer: format!("0x{}", hex::encode(signer.0)),
            domain: None,
            deferred_withdrawals: true,
        },
        liabilities: engine_core::Fixed::ZERO,
        deposits: 0,
        authorized: engine_core::Fixed::ZERO,
        coverage: crate::checkpoints::DepositCoverage::Historical,
        distributed: engine_core::UsdcUnits(0),
    };
    let replica = || {
        let mut replica = LiveReplay::new(DOMAIN, [outgoing].into());
        // The anchor and an unaccepted checkpoint by the outgoing key; two by the promoted key after it.
        for (seq, signer, after) in [(10, outgoing, 5), (20, outgoing, 10), (30, promoted, 10), (40, promoted, 30)] {
            replica.checkpoints.insert(seq, (work(seq, signer, after), 0));
        }
        replica
    };
    let mut first = replica();
    first.anchor(parent(10), Some(promoted)).unwrap();
    let seqs =
        |replica: &LiveReplay| replica.replay.recovery_work.iter().map(|work| work.checkpoint.seq).collect::<Vec<_>>();
    assert_eq!(first.replay.recovery_anchor_work.as_ref().map(|work| work.checkpoint.seq), Some(10));
    assert_eq!(seqs(&first), vec![30, 40], "the outgoing key's unaccepted checkpoint is retired");
    let refused = replica().anchor(parent(10), None).unwrap_err();
    assert!(format!("{refused:#}").contains("divergent"), "unfiltered, the two chains break: {refused:#}");
    let mut fresh = LiveReplay::new(DOMAIN, [outgoing].into());
    fresh.checkpoints.insert(10, (work(10, outgoing, 5), 0));
    fresh.checkpoints.insert(20, (work(20, outgoing, 10), 0));
    fresh.anchor(parent(10), Some(promoted)).unwrap();
    assert!(seqs(&fresh).is_empty(), "a first start resumes nothing the outgoing key signed");
}

/// A copy holding every command through the vault's checkpoint but not its entry, as a promotion
/// leaves one when no member received it (#595), anchors there from its own state: at the start's
/// tip, at a command sealed by another key, and in a full recovery's replay; only with the vault's root.
#[tokio::test]
async fn a_copy_lacking_the_vaults_checkpoint_entry_anchors_at_its_own_state_with_the_vaults_root() {
    let dir = scratch_dir("live-replica-unheld");
    sequence_ticks(&dir, 7).await;
    let entries = log_entries(&dir);
    // The copy ends at the command of the vault's latest checkpoint, without the entry.
    let at = entries.iter().rposition(|entry| matches!(entry, LogEntry::Checkpoint(_))).unwrap();
    let LogEntry::Checkpoint(accepted) = entries[at].clone() else { unreachable!() };
    let copy = &entries[..at];
    let vault = |root: B256| CheckpointParent { has_checkpoint: true, seq: accepted.seq, root };
    let (root, other) = (accepted.root.parse::<B256>().unwrap(), B256::repeat_byte(0xee));
    let fed = || {
        let mut replica = LiveReplay::new(DOMAIN, [signer()].into());
        copy.iter().for_each(|entry| replica.apply(entry).unwrap());
        replica
    };
    // At the start's tip.
    let mut tip = fed();
    assert!(tip.checkpoint_at(accepted.seq).is_none());
    tip.capture_tip();
    let held = tip.checkpoint_at(accepted.seq).cloned().expect("captured at the tip");
    assert_eq!(
        (held.seq, held.root.as_str(), held.signer.as_str()),
        (accepted.seq, accepted.root.as_str(), accepted.signer.as_str())
    );
    // Kept past the retention window when the start keeps its seq, as a checkpoint entry is.
    let mut kept = fed();
    kept.keep(accepted.seq);
    kept.capture_tip();
    assert!(kept.retained.kept_checkpoints.contains_key(&accepted.seq), "a kept seq's captured checkpoint is kept");
    let mut anchored = fed();
    anchored.capture_tip();
    anchored.anchor(vault(root), None).unwrap();
    assert_eq!(anchored.replay.recovery_anchor.as_ref().map(|cp| cp.seq), Some(accepted.seq));
    // Its root is the finished replay's checkpoint root there, as an entry's would be.
    assert_eq!(anchored.replay.finish().checkpoint_root(accepted.seq), Some(root));
    let refused = tip.anchor(vault(other), None).unwrap_err();
    assert!(format!("{refused:#}").contains("not among the checkpoints"), "another root: {refused:#}");
    // At a command sealed by another key, captured before it applies.
    let mut next = fed();
    let LogEntry::Command(mut command) = copy.last().cloned().unwrap() else { panic!("the copy ends at a command") };
    command.seal.as_mut().expect("sealed").signer = AccountId([0x77; 20]);
    let _ = next.apply(&LogEntry::Command(command));
    assert!(next.checkpoint_at(accepted.seq).is_some(), "another key's command captures the tip first");
    // In a full recovery's replay anchored at the vault's checkpoint, its roots computed by its engine
    // or on its root thread, which hands the tree back for the anchor's root.
    for ahead in [false, true] {
        let recovered = |root: B256| {
            let mut replay = Replay::with_recovery(Some(DOMAIN), Some(signer()), vault(root));
            if ahead {
                replay.check_roots_ahead();
            }
            copy.iter().for_each(|entry| replay.apply(entry).unwrap());
            replay.settled(Ok(())).unwrap();
            replay.finish()
        };
        let replayed = recovered(root);
        assert_eq!(replayed.checkpoint_root(accepted.seq), Some(root), "ahead: {ahead}");
        assert_eq!(replayed.recovery_anchor_work.map(|work| work.checkpoint.seq), Some(accepted.seq));
        let refused = recovered(other);
        assert!(refused.recovery_anchor_work.is_none(), "another root anchors nothing");
        assert_eq!(refused.checkpoint_root(accepted.seq), None, "nor counts as a checkpoint root");
    }
}

/// A full recovery under a promoted key passes the checkpoints the outgoing key signed past the vault's
/// latest: the vault never took them and never will, so they anchor and extend nothing, but only once
/// a later key's command follows them. Without the history naming the outgoing key in an earlier epoch,
/// such a checkpoint is refused at once.
#[tokio::test]
async fn a_recovery_under_a_promoted_key_passes_the_outgoing_keys_unaccepted_checkpoints() {
    let dir = scratch_dir("replay-superseded");
    sequence_ticks(&dir, 7).await;
    let entries = log_entries(&dir);
    let checkpoints: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint.clone()),
            _ => None,
        })
        .collect();
    assert!(checkpoints.len() >= 2, "an accepted checkpoint and a later one");
    let accepted = &checkpoints[0];
    let (outgoing, promoted) = (signer(), AccountId(EnclaveKey::generate().address()));
    let parent = CheckpointParent { has_checkpoint: true, seq: accepted.seq, root: accepted.root.parse().unwrap() };
    let recovered = |epochs: BTreeMap<u64, AccountId>| {
        let mut replay = Replay::with_recovery(Some(DOMAIN), Some(promoted), parent);
        let authority = crate::checkpoints::AcceptedCheckpoint {
            seq: accepted.seq,
            root: accepted.root.clone(),
            signer: outgoing,
            signer_epoch: 1,
        };
        replay.set_authorities(crate::checkpoints::SignerHistory { checkpoints: vec![authority], epochs })?;
        entries.iter().try_for_each(|entry| replay.apply(entry))?;
        anyhow::Ok(replay)
    };
    let passed = recovered(BTreeMap::from([(1, outgoing), (2, promoted)])).unwrap();
    assert!(passed.recovery_work.is_empty(), "the unaccepted checkpoints extend nothing");
    // This log ends at the outgoing key's checkpoint, as a rotation leaves one: no promoted key's
    // command follows it, so the recovery is refused.
    let ended = passed.ensure_superseded_continued().unwrap_err();
    assert!(format!("{ended:#}").contains("no later key's command follows it"), "{ended:#}");
    // A promoted key's command after it, as a promotion's copy holds one, settles it.
    let mut continued = recovered(BTreeMap::from([(1, outgoing), (2, promoted)])).unwrap();
    let mut next = entries.iter().rev().find_map(|entry| match entry {
        LogEntry::Command(command) => Some(command.clone()),
        _ => None,
    });
    next.as_mut().and_then(|command| command.seal.as_mut()).expect("sealed").signer = promoted;
    continued.before(&LogEntry::Command(next.unwrap())).unwrap();
    continued.ensure_superseded_continued().expect("a later key's command follows the checkpoint");
    assert_eq!(passed.recovery_anchor.as_ref().map(|cp| cp.seq), Some(accepted.seq));
    let refused = recovered(BTreeMap::from([(1, outgoing)])).err().expect("no later epoch names the promoted key");
    assert!(format!("{refused:#}").contains("trusted recovery authority"), "{refused:#}");
}

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
    // The ticks wait on the disk, which may keep them past trusted time's grace on a loaded machine.
    tokio::spawn(keep_time_fresh(node.tx.downgrade()));
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
