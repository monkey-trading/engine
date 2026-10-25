//! A cache of custody receipts restores each checkpoint's canonical coverage as full replay derives
//! it, and is refused where only full replay can derive legacy provenance.

use std::num::NonZeroUsize;
use std::sync::Arc;

use engine_core::Engine;
use engine_core::command::Command;
use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::types::AccountId;
use engine_core::{Fixed, protocol::ProtocolVersion};
use tokio::sync::{broadcast, mpsc, watch};

use crate::checkpoints::DepositCoverage;
use crate::clock::test_clock::TestClock;
use crate::log::{CheckpointDomain, Manifest, SegmentedLog};
use crate::sequencer::replay::Replay;
use crate::sequencer::{Sequencer, SequencerConfig, legacy};
use crate::snapshot;
use crate::tee::EnclaveKey;
use crate::test_support::{TRADER, dev_config, sample_around, scratch_dir, sequencer_config, withdraw};

/// A gated cache taken after a finalised frontier carries chain-counted deposits, which accounting
/// does not determine: reading it rebuilds the coverage from the engine, recovery from it matches
/// full replay, and a cache whose coverage claim differs is refused. A cache owing an approval made
/// before custody finality activated, or one from before activation, is refused for full replay.
#[test]
fn a_gated_cache_restores_canonical_coverage_and_refuses_inherited_provenance() {
    let dir = scratch_dir("snapshot-custody-coverage");
    let key = Arc::new(EnclaveKey::test_key());
    let signer = AccountId(key.address());
    let cache = crate::config::SnapshotSection {
        dir: scratch_dir("snapshot-custody-coverage-cache"),
        retain: NonZeroUsize::new(3).unwrap(),
        max_bytes: NonZeroUsize::new(32 * 1024 * 1024).unwrap(),
        trusted_signers: vec![signer],
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
    // The deferred-signature expectations below are V20's; V22 registers each approval in the
    // payout gate instead and refuses snapshot recovery while one is present.
    seq.activate_protocol_at(ProtocolVersion::V20).unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    while queued.try_recv().is_ok() {}
    seq.checkpoint_status(seq.last_seq(), true).unwrap();
    let parent = seq.snapshot_base.as_ref().unwrap().parent().unwrap();
    let (kept, pending) = (receipt(1, 30), receipt(2, 5));
    custody(&mut seq, Command::CustodyFinalised(frontier(0, 0)));
    custody(&mut seq, update(DepositUpdate::Credit { receipt: kept.clone() }));
    let observed_at_seq = seq.engine.recorded_deposit(&kept.reference()).unwrap().updated_at_seq;
    custody(&mut seq, update(DepositUpdate::Finalise { receipt: kept.clone(), observed_at_seq }));
    custody(&mut seq, Command::CustodyFinalised(frontier(kept.block, 1)));
    custody(&mut seq, update(DepositUpdate::Credit { receipt: pending }));
    let events = seq.sequence(withdraw(5)).unwrap().unwrap();
    assert!(events.iter().any(|event| matches!(event, Event::WithdrawalApproved { .. })));
    let mut last = None;
    while let Ok(work) = queued.try_recv() {
        last = Some(work);
    }
    let last = last.unwrap();
    assert_eq!((last.deposits, last.coverage), (2, DepositCoverage::Canonical));
    let offset = seq.outbox.last().unwrap().offset;
    let context = seq.recovery_context(seq.snapshot_base.clone().unwrap());
    assert_eq!(context.deferred.len(), 1, "the approval awaits its checkpoint");
    let captured = seq.snapshot_payload(last.checkpoint.clone(), seq.progress.clone(), context, offset).unwrap();
    seq.flush().unwrap();
    drop(seq);
    snapshot::write(&cache, &captured, &key).unwrap();
    let path = snapshot::candidates_readonly(&cache).unwrap().remove(0);

    let (cached, engine) = snapshot::read(&cache, &path, domain.chain_id, domain.vault).unwrap();
    assert_eq!(engine.custody_deposit_coverage(), Some(2), "reading a cache rebuilds derived coverage");
    let mut restored = Replay::from_snapshot(engine, cached, domain, Some(signer), Some(parent)).unwrap().finish();
    let mut full = Replay::with_recovery(Some(domain), Some(signer), parent);
    SegmentedLog::open(&dir, |entry| full.apply(&entry)).unwrap();
    let mut full = full.finish();
    assert_eq!(restored.engine.state_root(), full.engine.state_root());
    assert_eq!(restored.engine.custody_deposit_coverage(), full.engine.custody_deposit_coverage());
    assert_eq!(restored.accounting, full.accounting);
    assert_eq!(restored.deferred, full.deferred);
    let work = |replayed: &crate::sequencer::Replayed| serde_json::to_value(&replayed.recovery_work).unwrap();
    assert_eq!(work(&restored), work(&full), "each checkpoint keeps the coverage it was signed with");
    assert!(restored.legacy_deposits.is_empty() && full.legacy_deposits.is_empty());

    let (mut inherited, engine) = snapshot::read(&cache, &path, domain.chain_id, domain.vault).unwrap();
    let approved_at = inherited.context.deferred[0].0;
    for (version, seq) in &mut inherited.progress.activations {
        if version.supports_deposit_finality() {
            *seq = approved_at + 1;
        }
    }
    let refused = Replay::from_snapshot(engine, inherited, domain, Some(signer), Some(parent)).err().unwrap();
    assert!(refused.to_string().contains("before custody finality"), "{refused:#}");
    let unactivated = legacy::check_snapshot_provenance(&Engine::new(), &[], &[]).unwrap_err();
    assert!(unactivated.to_string().contains("predates custody finality"), "{unactivated:#}");
    let activated = [(ProtocolVersion::V20, approved_at)];
    legacy::check_snapshot_provenance(&restored.engine, &activated, &restored.deferred).unwrap();

    let (mut tampered, _) = snapshot::read(&cache, &path, domain.chain_id, domain.vault).unwrap();
    tampered.context.work.last_mut().unwrap().deposits += 1;
    snapshot::write(&cache, &tampered, &key).unwrap();
    let path = snapshot::candidates_readonly(&cache).unwrap().remove(0);
    let mismatch = snapshot::read(&cache, &path, domain.chain_id, domain.vault).err().unwrap();
    assert!(mismatch.to_string().contains("accounting mismatch"), "{mismatch:#}");
}

/// A cache whose engine holds a V22 withdrawal approval refuses snapshot recovery: the cache does
/// not carry payout delivery order, so recovery replays the full log instead.
#[test]
fn a_cache_holding_a_v22_approval_refuses_snapshot_recovery() {
    let dir = scratch_dir("snapshot-v22-approval");
    let key = Arc::new(EnclaveKey::test_key());
    let signer = AccountId(key.address());
    let cache = crate::config::SnapshotSection {
        dir: scratch_dir("snapshot-v22-approval-cache"),
        retain: NonZeroUsize::new(3).unwrap(),
        max_bytes: NonZeroUsize::new(32 * 1024 * 1024).unwrap(),
        trusted_signers: vec![signer],
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
    seq.checkpoint_status(seq.last_seq(), true).unwrap();
    let parent = seq.snapshot_base.as_ref().unwrap().parent().unwrap();
    custody(
        &mut seq,
        Command::Deposit { account: TRADER, amount: Fixed::from_int(30), chain_ref: "dev:known-funds".into() },
    );
    let events = seq.sequence(withdraw(5)).unwrap().unwrap();
    assert!(events.iter().any(|event| matches!(event, Event::WithdrawalApproved { .. })));
    assert!(seq.engine.withdrawal_approvals().next().is_some(), "the approval is a permanent engine leaf under V22");
    let mut last = None;
    while let Ok(work) = queued.try_recv() {
        last = Some(work);
    }
    let last = last.unwrap();
    let offset = seq.outbox.last().unwrap().offset;
    let context = seq.recovery_context(seq.snapshot_base.clone().unwrap());
    assert!(context.deferred.is_empty(), "V22 approvals wait in the payout gate, not the deferred list");
    let captured = seq.snapshot_payload(last.checkpoint.clone(), seq.progress.clone(), context, offset).unwrap();
    seq.flush().unwrap();
    drop(seq);
    snapshot::write(&cache, &captured, &key).unwrap();
    let path = snapshot::candidates_readonly(&cache).unwrap().remove(0);
    let (cached, engine) = snapshot::read(&cache, &path, domain.chain_id, domain.vault).unwrap();
    let refused = Replay::from_snapshot(engine, cached, domain, Some(signer), Some(parent)).err().unwrap();
    assert!(refused.to_string().contains("cannot restore withdrawal payout order"), "{refused:#}");
}

fn custody(seq: &mut Sequencer, cmd: Command) {
    let events = seq.sequence(cmd).unwrap().unwrap();
    assert!(
        !events.iter().any(|event| matches!(event, Event::AdminRejected { .. } | Event::CustodyDepositRejected { .. }))
    );
}

fn update(update: DepositUpdate) -> Command {
    Command::CustodyDeposit(Box::new(update))
}

fn frontier(block: u64, deposit_count: u64) -> CustodyCheckpoint {
    CustodyCheckpoint { block, block_hash: B256::repeat_byte(0x40 + block as u8), deposit_count }
}

fn receipt(id: u64, amount: i64) -> DepositReceipt {
    DepositReceipt {
        block: 10 + id,
        block_hash: B256::repeat_byte(id as u8),
        tx_hash: B256::repeat_byte(0x10 + id as u8),
        log_index: id,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary: DepositBeneficiary::Trader { account: TRADER },
        amount: Fixed::from_int(amount),
    }
}
