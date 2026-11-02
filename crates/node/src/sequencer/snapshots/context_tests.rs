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
use crate::test_support::{dev_config, sample_around, scratch_dir, sequencer_config};

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
    seq.activate_legacy_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(crate::test_support::empty_custody_frontier()).unwrap();
    while queued.try_recv().is_ok() {}
    let base = seq.last_seq().unwrap();
    seq.checkpoint_status(Some(base), Some(crate::checkpoints::Health::Healthy)).unwrap();
    let parent = seq.snapshot_base.as_ref().unwrap().parent().unwrap();
    let mut expected = Vec::new();
    for amount in [7, 13] {
        let receipt = engine_core::deposits::DepositReceipt {
            block: 1,
            block_hash: engine_core::eip712::B256::repeat_byte(1),
            tx_hash: engine_core::eip712::B256::repeat_byte(amount as u8),
            log_index: 0,
            transaction_deposit_index: 0,
            deposit_id: expected.len() as u64,
            beneficiary: engine_core::deposits::DepositBeneficiary::House,
            amount: Fixed::from_int(amount),
        };
        seq.stamp(
            seq.last_ts,
            engine_core::command::Command::CustodyDeposit(Box::new(engine_core::deposits::DepositUpdate::Credit {
                receipt,
            })),
        )
        .unwrap();
        expected.push(queued.try_recv().unwrap());
    }
    let offset = seq.outbox.last().unwrap().offset;
    let context = seq.recovery_context(seq.snapshot_base.clone().unwrap());
    assert_eq!(
        context.base_work.as_ref().unwrap().checkpoint.seq,
        base,
        "the authenticated cache retains anchor accounting"
    );
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
    assert_eq!(replayed.recovery_anchor_work.as_ref().unwrap().checkpoint.seq, base);
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
    let command = crate::log::CommandEntry { seal: None, sequenced, events: Some(events) };
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
            base_work: None,
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

impl ReplayedLog {
    /// Authenticates and restores the exact full-replay work through the production snapshot reader.
    pub(crate) fn test_authenticated_snapshot(
        self,
        cache: &crate::config::SnapshotSection,
        dir: &std::path::Path,
        parent: crate::log::CheckpointParent,
    ) -> Self {
        let key = EnclaveKey::test_key();
        let signer = AccountId(key.address());
        let replayed = &self.replayed;
        let checkpoint = replayed.last_signed_checkpoint.clone().unwrap();
        let params_seq = replayed.params_seq;
        let domain = checkpoint.domain.unwrap();
        assert!(replayed.recovery_anchor_work.as_ref().unwrap().has_proven_prefix());
        assert!(replayed.recovery_work.iter().any(|work| !work.has_proven_prefix()));
        let offset = crate::test_support::log_entries(dir)
            .iter()
            .fold(None, |previous, entry| Some(crate::log::offset_of(previous, entry).unwrap()))
            .unwrap();
        let payload = Snapshot {
            schema: 2,
            pair: None,
            chain_id: domain.chain_id,
            vault: domain.vault,
            checkpoint: checkpoint.clone(),
            offset,
            engine: serde_json::to_string(&replayed.engine).unwrap(),
            progress: snapshot::Progress {
                genesis: replayed.genesis.clone(),
                activations: replayed.activations.clone(),
                checkpoints: replayed.checkpoints,
                params_seq: replayed.params_seq,
            },
            context: snapshot::RecoveryContext {
                commands: replayed.commands,
                command_prefix: replayed.command_prefix,
                accounting: replayed.accounting,
                logs_events: true,
                unsigned: replayed.unsigned.clone(),
                deferred: replayed.deferred.clone(),
                checkpoint_signers: replayed.checkpoint_signers.clone(),
                base: snapshot::RecoveryBase::Checkpoint {
                    checkpoint: replayed.recovery_anchor_work.as_ref().unwrap().checkpoint.clone(),
                },
                base_work: replayed.recovery_anchor_work.clone(),
                work: replayed.recovery_work.clone(),
            },
        };
        snapshot::write(cache, &payload, &key).unwrap();
        let path = snapshot::candidates_readonly(cache).unwrap().remove(0);
        let (cached, engine) = snapshot::read(cache, &path, domain.chain_id, domain.vault).unwrap();
        let replayed = Replay::from_snapshot(engine, cached, domain, Some(signer), Some(parent)).unwrap().finish();
        assert_eq!(replayed.last_checkpoint_offset, Some(offset), "a snapshot restores where its checkpoint sits");
        assert_eq!(replayed.params_seq, params_seq, "a snapshot carries the seq of the parameters in force");
        assert_eq!(replayed.recovery_work.last().unwrap().checkpoint, checkpoint);
        Self { replayed, snapshot_seq: Some(checkpoint.seq), ..self }
    }
}
