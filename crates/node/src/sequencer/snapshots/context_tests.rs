//! Cache recovery must preserve the concrete work that finality reconciliation consumes.

use std::num::NonZeroUsize;
use std::sync::Arc;

use engine_core::Fixed;
use engine_core::types::AccountId;
use tokio::sync::{broadcast, mpsc, watch};

use crate::clock::test_clock::TestClock;
use crate::log::{CheckpointDomain, Manifest, SegmentedLog};
use crate::sequencer::replay::Replay;
use crate::sequencer::{ReplayedLog, Sequencer, SequencerConfig};
use crate::snapshot::{self, Snapshot};
use crate::tee::EnclaveKey;
use crate::test_support::{deposit, dev_config, sample_around, scratch_dir, sequencer_config};

/// A cache captured at a checkpoint restores each outstanding checkpoint's work with the accounting
/// it was signed with, under a freshly supplied finalized anchor, and a cache missing that work
/// is refused.
#[test]
fn cache_restores_each_outstanding_checkpoint_with_its_original_accounting() {
    let dir = scratch_dir("snapshot-concrete-work");
    let key = Arc::new(EnclaveKey::test_key());
    let cache = crate::config::SnapshotSection {
        dir: scratch_dir("snapshot-concrete-work-cache"),
        retain: NonZeroUsize::new(3).unwrap(),
        max_bytes: NonZeroUsize::new(32 * 1024 * 1024).unwrap(),
        trusted_signers: vec![AccountId(key.address())],
    };
    let clock = Arc::new(TestClock::at(1_792_504_800_000));
    let (sink, mut queued) = mpsc::channel(128);
    let cfg = SequencerConfig {
        clock: clock.clone(),
        checkpoint_every: 1,
        checkpoints: Some(sink),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let domain = CheckpointDomain { chain_id: cfg.chain_id, vault: cfg.vault };
    let mut seq = Sequencer::recover(cfg, key.clone(), broadcast::channel(32).0, mpsc::unbounded_channel().0).unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    while queued.try_recv().is_ok() {}
    let base = seq.last_seq().unwrap();
    seq.checkpoint_status(Some(base), true).unwrap();
    let parent = seq.snapshot_base.as_ref().unwrap().parent().unwrap();
    let mut expected = Vec::new();
    for (amount, reference) in [(7, "0xfirst"), (13, "0xsecond")] {
        seq.stamp(seq.last_ts, deposit(amount, reference)).unwrap();
        expected.push(queued.try_recv().unwrap());
    }
    let offset = seq.outbox.last().unwrap().offset;
    let context = seq.recovery_context(seq.snapshot_base.clone().unwrap());
    let captured = seq.snapshot_payload(expected[1].checkpoint.clone(), seq.progress.clone(), context, offset).unwrap();
    seq.flush().unwrap();
    let (accounting, command_prefix) = (seq.accounting, seq.command_prefix);
    drop(seq);
    assert_eq!(expected[1].liabilities - expected[0].liabilities, Fixed::from_int(13));
    assert_eq!(expected[1].deposits, expected[0].deposits + 1);
    snapshot::write(&cache, &captured, &key).unwrap();
    let path = snapshot::candidates_readonly(&cache).unwrap().remove(0);
    let (cached, engine) = snapshot::read(&cache, &path, domain.chain_id, domain.vault).unwrap();
    let replayed =
        Replay::from_snapshot(engine, cached, domain, Some(AccountId(key.address())), Some(parent)).unwrap().finish();
    assert_eq!(replayed.accounting, accounting);
    assert_eq!(replayed.command_prefix, command_prefix);
    let actual = &replayed.recovery_work;
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(&expected) {
        assert_eq!(actual.checkpoint, expected.checkpoint);
        assert_eq!((actual.liabilities, actual.deposits), (expected.liabilities, expected.deposits));
    }
    let restored = ReplayedLog {
        log: SegmentedLog::open(&dir, |_| Ok(())).unwrap(),
        replayed,
        snapshot_seq: None,
        legacy_authorizations: Default::default(),
    };
    restored.validate_submission(parent, Some(&expected[0].checkpoint)).unwrap();
    let mut reserved = expected[0].checkpoint.clone();
    reserved.signature.push('0');
    assert!(restored.validate_submission(parent, Some(&reserved)).is_err());
    let (mut tampered, _) = snapshot::read(&cache, &path, domain.chain_id, domain.vault).unwrap();
    tampered.context.work.remove(0);
    snapshot::write(&cache, &tampered, &key).unwrap();
    let path = snapshot::candidates_readonly(&cache).unwrap().remove(0);
    assert!(
        snapshot::read(&cache, &path, domain.chain_id, domain.vault).is_err(),
        "a digest cannot supply the missing work"
    );
}

#[test]
fn cache_context_requires_work_and_all_accounting_fields() {
    let key = EnclaveKey::test_key();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    let mut engine = engine_core::Engine::new();
    let sequenced = engine_core::command::Sequenced { seq: 0, ts: 0, cmd: engine_core::command::Command::Tick };
    let events = engine.apply(&sequenced).unwrap();
    let command = crate::log::CommandEntry { sequenced, events: Some(events) };
    let command_prefix = crate::log::extend_command_prefix([0; 32], crate::log::command_digest(&command).unwrap());
    let root = engine.state_root();
    let cp = crate::log::Checkpoint {
        seq: 0,
        root: format!("0x{}", hex::encode(root)),
        signer: key.address_hex(),
        signature: key.sign_checkpoint(domain.chain_id, &domain.vault.0, 0, &root),
        domain: Some(domain),
        parent: None,
        deferred_withdrawals: false,
    };
    let snapshot = Snapshot {
        schema: 2,
        pair: None,
        chain_id: domain.chain_id,
        vault: domain.vault,
        checkpoint: cp,
        offset: crate::log::Offset { seq: 0, index: 1 },
        engine: serde_json::to_string(&engine).unwrap(),
        progress: snapshot::Progress::default(),
        context: snapshot::RecoveryContext {
            commands: 1,
            command_prefix,
            accounting: crate::checkpoints::accounting::Accounting::default(),
            logs_events: true,
            unsigned: Vec::new(),
            deferred: Vec::new(),
            checkpoint_signers: [AccountId(key.address())].into(),
            base: snapshot::RecoveryBase::Ungated,
            work: Vec::new(),
        },
    };
    let value = serde_json::to_value(snapshot).unwrap();
    for field in ["liabilities", "deposits", "unpaid"] {
        let mut missing = value.clone();
        missing["context"]["accounting"].as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<Snapshot>(missing).is_err(), "missing {field} must never become zero");
    }
    for field in
        ["work", "base", "commands", "command_prefix", "logs_events", "unsigned", "deferred", "checkpoint_signers"]
    {
        let mut missing = value.clone();
        missing["context"].as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<Snapshot>(missing).is_err(),
            "missing {field} must not restore an empty context"
        );
    }
    let mut missing = value;
    missing.as_object_mut().unwrap().remove("context");
    assert!(serde_json::from_value::<Snapshot>(missing).is_err());
}
