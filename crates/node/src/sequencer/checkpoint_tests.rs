//! Withdrawal signatures must stay inside the enclave until their approval is committed on chain.

use std::sync::Arc;

use engine_core::Fixed;
use tokio::sync::{broadcast, mpsc, watch};

use crate::checkpoints::Health;
use crate::log::{LogEntry, Manifest};
use crate::sequencer::Sequencer;
use crate::sequencer::fixture_tests::replay_trusted as replay;
use crate::tee::EnclaveKey;
use crate::test_support::{deposit, dev_config, log_entries, scratch_dir, sequencer_config, trusted_now, withdraw};

#[test]
fn only_a_reconciled_posted_prefix_releases_signatures_and_recovery_keeps_the_gate() {
    let dir = scratch_dir("checkpoint-gate");
    let (work, mut queued) = mpsc::channel(64);
    let (withdrawals, mut signed) = mpsc::unbounded_channel();
    let (stream, _) = broadcast::channel(128);
    let key = Arc::new(EnclaveKey::test_key());
    let cfg = crate::sequencer::SequencerConfig {
        checkpoint_parent: Default::default(),
        checkpoints: Some(work.clone()),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(cfg, key.clone(), stream.clone(), withdrawals.clone()).unwrap();
    // Before V22, an approval is signed once its checkpoint is posted and reconciled.
    seq.activate_protocol_at(engine_core::protocol::ProtocolVersion::V18).unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    while queued.try_recv().is_ok() {}
    let before = seq.last_seq();
    assert!(seq.sequence(withdraw(1)).unwrap().is_none(), "initial reconciliation is required");
    assert_eq!(seq.last_seq(), before);
    seq.checkpoint_status(seq.last_seq(), Some(crate::checkpoints::Health::Healthy)).unwrap();
    seq.sequence(deposit(100, "dep-gate")).unwrap();
    seq.sequence(withdraw(40)).unwrap();
    let approved = seq.last_seq().unwrap();
    let mut approval_work = None;
    while let Ok(work) = queued.try_recv() {
        if work.checkpoint.seq == approved {
            approval_work = Some(work);
        }
    }
    assert_eq!(approval_work.unwrap().liabilities, dev_config().genesis.house_capital + Fixed::from_int(100));
    seq.flush().unwrap();
    assert!(signed.try_recv().is_err());
    let entries = log_entries(&dir);
    assert!(!entries.iter().any(|entry| matches!(entry, LogEntry::Withdrawal(_))));
    let replayed = replay(&entries).unwrap();
    assert_eq!(replayed.deferred.len(), 1);
    assert_eq!(replayed.unpaid, Fixed::from_int(40));
    seq.sequence(engine_core::command::Command::Tick).unwrap();
    seq.flush().unwrap();
    seq.checkpoint_status(Some(approved - 1), Some(crate::checkpoints::Health::Healthy)).unwrap();
    seq.checkpoint_status(Some(approved), Some(crate::checkpoints::Health::Pending)).unwrap();
    seq.flush().unwrap();
    assert!(signed.try_recv().is_err());
    drop(seq);
    // Recovery must preserve a delayed approval even after later commands exist.
    let cfg = crate::sequencer::SequencerConfig {
        checkpoint_parent: Default::default(),
        checkpoints: Some(work),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(cfg, key, stream, withdrawals).unwrap();
    assert!(signed.try_recv().is_err());
    seq.checkpoint_status(Some(approved), Some(crate::checkpoints::Health::Healthy)).unwrap();
    seq.flush().unwrap();
    let signature = signed.try_recv().unwrap();
    assert_eq!(signature.approved_seq, Some(approved));
    assert!(replay(&log_entries(&dir)).unwrap().deferred.is_empty());
}

#[test]
fn recovering_a_completed_archive_waits_for_a_command_before_appending_a_checkpoint() {
    use crate::log::{Checkpoint, CheckpointDomain, CommandEntry, SegmentedLog};
    use engine_core::Engine;
    use engine_core::command::{Command, Sequenced};
    use engine_core::types::AccountId;

    let dir = scratch_dir("checkpoint-archive-recovery");
    let key = Arc::new(EnclaveKey::test_key());
    let mut engine = Engine::new();
    let command = Sequenced { seq: 0, ts: 0, cmd: Command::Tick };
    let events = engine.apply(&command).unwrap();
    let root = engine.state_root();
    let mut log =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(1, watch::channel(Manifest::default()).0).unwrap();
    log.append(&LogEntry::Command(CommandEntry { seal: None, sequenced: command, events: Some(events) })).unwrap();
    log.append(&LogEntry::Checkpoint(Checkpoint {
        parent: None,
        seq: 0,
        root: format!("0x{}", hex::encode(root)),
        signature: key.sign_checkpoint(31337, &[0; 20], 0, &root),
        signer: key.address_hex(),
        domain: Some(CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) }),
        deferred_withdrawals: true,
    }))
    .unwrap();
    log.append(&LogEntry::Command(CommandEntry {
        seal: None,
        sequenced: Sequenced { seq: 1, ts: 0, cmd: Command::Tick },
        events: Some(Vec::new()),
    }))
    .unwrap();
    log.flush().unwrap();
    drop(log);
    std::fs::remove_file(dir.join("00000000000000000001.jsonl")).unwrap();
    let (work, _queued) = mpsc::channel(64);
    let cfg = crate::sequencer::SequencerConfig {
        checkpoint_parent: Default::default(),
        checkpoints: Some(work),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(cfg, key, broadcast::channel(16).0, mpsc::unbounded_channel().0).unwrap();
    assert_eq!(seq.last_seq(), Some(0));
    seq.trust(trusted_now()).unwrap();
    seq.sequence(Command::Tick).unwrap();
    seq.flush().unwrap();
    assert_eq!(replay(&log_entries(&dir)).unwrap().engine.last_seq(), Some(1));
}

#[test]
fn gated_checkpoints_sign_the_previous_queued_root_and_coalesce_duplicate_requests() {
    let dir = scratch_dir("checkpoint-parent-chain");
    let (work, mut queued) = mpsc::channel(64);
    let cfg = crate::sequencer::SequencerConfig {
        checkpoints: Some(work),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(
        cfg,
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(128).0,
        mpsc::unbounded_channel().0,
    )
    .unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    seq.checkpoint().unwrap();
    seq.checkpoint().unwrap();
    let mut prior: Option<crate::log::Checkpoint> = None;
    let mut count = 0;
    while let Ok(work) = queued.try_recv() {
        let cp = work.checkpoint;
        let parent = cp.parent.unwrap();
        if let Some(prior) = prior {
            assert!(parent.has_checkpoint);
            assert_eq!(parent.seq, prior.seq);
            assert_eq!(parent.root.to_string(), prior.root);
            assert!(cp.seq > prior.seq);
        } else {
            assert_eq!(parent, Default::default());
        }
        crate::checkpoints::signature::verify_domain(&cp, cp.domain.unwrap()).unwrap();
        prior = Some(cp);
        count += 1;
    }
    assert!(count > 0);
    assert_eq!(prior.unwrap().seq, seq.last_seq().unwrap());
    seq.checkpoint().unwrap();
    assert!(queued.try_recv().is_err());
}

#[test]
fn restart_resumes_exact_unmined_chain_and_original_checkpoint_accounting() {
    let dir = scratch_dir("checkpoint-unmined-recovery");
    let key = Arc::new(EnclaveKey::test_key());
    let (sink, mut queued) = mpsc::channel(64);
    let config = || crate::sequencer::SequencerConfig {
        checkpoints: Some(sink.clone()),
        checkpoint_every: 1,
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq =
        Sequencer::recover(config(), key.clone(), broadcast::channel(128).0, mpsc::unbounded_channel().0).unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    let mut prior = Vec::new();
    while let Ok(work) = queued.try_recv() {
        prior.push(work);
    }
    let finalized = prior.last().unwrap().checkpoint.clone();
    seq.checkpoint_status(Some(finalized.seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    seq.sequence(deposit(100, "pending-checkpoint-deposit")).unwrap();
    seq.sequence(withdraw(40)).unwrap();
    seq.sequence(deposit(20, "later-deposit")).unwrap();
    seq.flush().unwrap();
    let mut outstanding = Vec::new();
    while let Ok(work) = queued.try_recv() {
        outstanding.push(work);
    }
    assert!(outstanding.len() >= 3);
    assert_ne!(outstanding.first().unwrap().liabilities, outstanding.last().unwrap().liabilities);
    let last = outstanding.last().unwrap().checkpoint.clone();
    let bytes = log_entries(&dir).len();
    drop(seq);
    let cfg = crate::sequencer::SequencerConfig {
        checkpoint_parent: crate::log::CheckpointParent {
            has_checkpoint: true,
            seq: finalized.seq,
            root: finalized.root.parse().unwrap(),
        },
        ..config()
    };
    let mut recovered = Sequencer::recover(cfg, key, broadcast::channel(128).0, mpsc::unbounded_channel().0).unwrap();
    let restored = recovered.take_recovery_checkpoints();
    assert_eq!(restored.len(), outstanding.len() + 1);
    assert_eq!(restored[0].checkpoint, finalized, "the worker revalidates finalized anchor accounting");
    assert_eq!(restored[0].liabilities, prior.last().unwrap().liabilities);
    for (actual, expected) in restored.iter().skip(1).zip(&outstanding) {
        assert_eq!(
            serde_json::to_value(&actual.checkpoint).unwrap(),
            serde_json::to_value(&expected.checkpoint).unwrap()
        );
        assert_eq!(actual.liabilities, expected.liabilities);
        assert_eq!(actual.deposits, expected.deposits);
    }
    assert_eq!(log_entries(&dir).len(), bytes, "restart must not sign a sibling");
    recovered.trust(trusted_now()).unwrap();
    // Recovery revalidates collateral before signing: the worker's first reconciliation lifts
    // the hold, then the cadence resumes.
    recovered.checkpoint_status(None, Some(crate::checkpoints::Health::Healthy)).unwrap();
    recovered.sequence(engine_core::command::Command::Tick).unwrap();
    let child = queued.try_recv().unwrap().checkpoint;
    assert_eq!(child.parent.unwrap().seq, last.seq);
    assert_eq!(child.parent.unwrap().root.to_string(), last.root);
}

#[test]
fn no_checkpoint_is_signed_while_collateral_is_diverged_and_signing_resumes_when_healthy() {
    // A signed checkpoint is public: anyone may relay it to the vault, and its root makes
    // distributions claimable. While collateral reconciliation is diverged no new root may exist
    // for a claim or a relay to reach; a Healthy verdict resumes the cadence.
    let dir = scratch_dir("checkpoint-sign-gate");
    let (work, mut queued) = mpsc::channel(64);
    let (withdrawals, _signed) = mpsc::unbounded_channel();
    let (stream, _) = broadcast::channel(128);
    let key = Arc::new(EnclaveKey::test_key());
    let cfg = crate::sequencer::SequencerConfig {
        checkpoint_parent: Default::default(),
        checkpoints: Some(work.clone()),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(cfg, key, stream, withdrawals).unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    while queued.try_recv().is_ok() {}
    let checkpoints = |dir: &std::path::Path| {
        log_entries(dir).iter().filter(|entry| matches!(entry, LogEntry::Checkpoint(_))).count()
    };
    // Before any collateral verdict — a recovered deployment whose poster has not observed the
    // vault yet — signing proceeds; only a Diverged verdict stops it, and the poster's read-only
    // withhold covers divergence it sees before any verdict reaches this gate.
    seq.sequence(deposit(100, "dep-sign-gate")).unwrap();
    for _ in 0..3 {
        if checkpoints(&dir) > 0 {
            break;
        }
        seq.sequence(engine_core::command::Command::Tick).unwrap();
    }
    assert!(checkpoints(&dir) > 0, "checkpoints are signed before any collateral verdict");
    seq.checkpoint_status(seq.last_seq(), Some(Health::Healthy)).unwrap();
    let before = checkpoints(&dir);
    seq.sequence(deposit(100, "dep-sign-gate-2")).unwrap();
    for _ in 0..3 {
        if checkpoints(&dir) > before {
            break;
        }
        seq.sequence(engine_core::command::Command::Tick).unwrap();
    }
    let healthy = checkpoints(&dir);
    assert!(healthy > before, "checkpoints are written while collateral reconciles");

    seq.checkpoint_status(None, Some(Health::Diverged)).unwrap();
    for _ in 0..4 {
        seq.sequence(engine_core::command::Command::Tick).unwrap();
        while queued.try_recv().is_ok() {}
    }
    assert_eq!(checkpoints(&dir), healthy, "no checkpoint is signed while collateral is diverged");

    seq.checkpoint_status(None, Some(Health::Healthy)).unwrap();
    for _ in 0..4 {
        seq.sequence(engine_core::command::Command::Tick).unwrap();
        while queued.try_recv().is_ok() {}
    }
    assert!(checkpoints(&dir) > healthy, "the cadence resumes once collateral reconciles");
}

#[test]
fn a_recovered_finalized_anchor_is_reconciled_before_any_new_signature() {
    let dir = scratch_dir("checkpoint-revalidate-anchor");
    let key = Arc::new(EnclaveKey::test_key());
    let (sink, mut queued) = mpsc::channel(64);
    let config = || crate::sequencer::SequencerConfig {
        checkpoints: Some(sink.clone()),
        checkpoint_every: 1,
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq =
        Sequencer::recover(config(), key.clone(), broadcast::channel(128).0, mpsc::unbounded_channel().0).unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    let mut anchor = None;
    while let Ok(work) = queued.try_recv() {
        anchor = Some(work);
    }
    let anchor = anchor.unwrap();
    seq.checkpoint_status(Some(anchor.checkpoint.seq), Some(crate::checkpoints::Health::Diverged)).unwrap();
    seq.flush().unwrap();
    drop(seq);
    let config = crate::sequencer::SequencerConfig {
        checkpoint_parent: crate::log::CheckpointParent {
            has_checkpoint: true,
            seq: anchor.checkpoint.seq,
            root: anchor.checkpoint.root.parse().unwrap(),
        },
        ..config()
    };
    let mut seq = Sequencer::recover(config, key, broadcast::channel(128).0, mpsc::unbounded_channel().0).unwrap();
    let recovered = seq.take_recovery_checkpoints();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].checkpoint, anchor.checkpoint);
    assert_eq!(recovered[0].liabilities, anchor.liabilities);
    seq.trust(trusted_now()).unwrap();
    seq.sequence(Command::Tick).unwrap();
    assert!(queued.try_recv().is_err(), "no signature before the recovered anchor is checked");
    seq.checkpoint_status(Some(anchor.checkpoint.seq), Some(crate::checkpoints::Health::Diverged)).unwrap();
    seq.sequence(Command::Tick).unwrap();
    assert!(queued.try_recv().is_err(), "proved divergence remains blocked");
    seq.checkpoint_status(Some(anchor.checkpoint.seq), Some(crate::checkpoints::Health::Pending)).unwrap();
    seq.sequence(Command::Tick).unwrap();
    assert!(queued.try_recv().is_ok(), "validated deposit catch-up may sign its clearing checkpoint");
}
