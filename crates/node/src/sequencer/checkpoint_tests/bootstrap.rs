//! Bootstrap anchors have a provable empty prefix; nonempty prefixes wait for canonical scanning.

use super::unvalidated::{Comparison, worker_status};
use super::*;
use crate::checkpoints::{DepositCoverage, Work};
use crate::log::CheckpointParent;
use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712::B256;

#[tokio::test]
async fn recovered_empty_bootstrap_anchor_reconciles_after_custody_catches_up() {
    let dir = scratch_dir("empty-bootstrap-restart");
    let key = Arc::new(EnclaveKey::test_key());
    let (sink, mut queued) = mpsc::channel(128);
    let mut genesis = dev_config().genesis;
    genesis.house_capital = Fixed::ZERO;
    let config = || crate::sequencer::SequencerConfig {
        checkpoints: Some(sink.clone()),
        checkpoint_every: 1,
        genesis: crate::sequencer::genesis_commands(&genesis),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq =
        Sequencer::recover(config(), key.clone(), broadcast::channel(128).0, mpsc::unbounded_channel().0).unwrap();
    seq.activate_protocol_at(engine_core::protocol::ProtocolVersion::V23).unwrap();
    seq.genesis(&genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    let anchor = drain(&mut queued);
    assert_eq!(anchor.coverage, DepositCoverage::AwaitingCanonicalScan);
    assert_eq!(anchor.liabilities, Fixed::ZERO);
    seq.flush().unwrap();
    drop(seq);
    let cfg = crate::sequencer::SequencerConfig {
        checkpoint_parent: CheckpointParent {
            has_checkpoint: true,
            seq: anchor.checkpoint.seq,
            root: anchor.checkpoint.root.parse().unwrap(),
        },
        ..config()
    };
    let mut seq = Sequencer::recover(cfg, key, broadcast::channel(128).0, mpsc::unbounded_channel().0).unwrap();
    let recovered = seq.take_recovery_checkpoints();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].coverage, DepositCoverage::AwaitingCanonicalScan);
    seq.trust(trusted_now()).unwrap();
    let (posted, health) = worker_status(recovered[0].clone(), Comparison::MissingFinalized).await;
    seq.checkpoint_status(posted, health).unwrap();
    seq.sequence(Command::Tick).unwrap();
    assert!(queued.try_recv().is_err(), "missing finalized observation must keep the recovery hold");
    custody_catchup(&mut seq);
    assert!(queued.try_recv().is_err(), "custody catch-up alone cannot release the signing hold");
    let (posted, health) = worker_status(recovered[0].clone(), Comparison::ValidatedCatchup).await;
    assert_eq!(health, Some(Health::Pending), "empty prefix is compared against actual finalized collateral");
    seq.checkpoint_status(posted, health).unwrap();
    seq.sequence(Command::Tick).unwrap();
    let successor = drain(&mut queued);
    assert_eq!(successor.coverage, DepositCoverage::Canonical);
    assert_eq!(successor.deposits, 1);
    assert!(successor.checkpoint.seq > anchor.checkpoint.seq);
}

#[test]
fn fresh_nonempty_unscanned_work_is_not_published() {
    let dir = scratch_dir("nonempty-bootstrap-waits");
    let (sink, mut queued) = mpsc::channel(128);
    let mut genesis = dev_config().genesis;
    genesis.house_capital = Fixed::ZERO;
    let cfg = crate::sequencer::SequencerConfig {
        checkpoints: Some(sink),
        checkpoint_every: 1,
        genesis: crate::sequencer::genesis_commands(&genesis),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(
        cfg,
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(128).0,
        mpsc::unbounded_channel().0,
    )
    .unwrap();
    seq.activate_protocol_at(engine_core::protocol::ProtocolVersion::V23).unwrap();
    seq.genesis(&genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    drain(&mut queued);
    credit(&mut seq);
    assert!(queued.try_recv().is_err(), "nonempty unscanned work must never become a recovery anchor");
    finalise(&mut seq);
    let work = drain(&mut queued);
    assert_eq!(work.coverage, DepositCoverage::Canonical);
    assert_eq!(work.liabilities, Fixed::from_int(10));
}

/// A gated sequencer that checkpoints every command, after its bootstrap, with a work queue of `capacity`.
fn gated(name: &str, capacity: usize) -> (Sequencer, mpsc::Receiver<Work>) {
    let dir = scratch_dir(name);
    let (sink, mut queued) = mpsc::channel(capacity);
    let mut genesis = dev_config().genesis;
    genesis.house_capital = Fixed::ZERO;
    let cfg = crate::sequencer::SequencerConfig {
        checkpoints: Some(sink),
        checkpoint_every: 1,
        genesis: crate::sequencer::genesis_commands(&genesis),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(
        cfg,
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(128).0,
        mpsc::unbounded_channel().0,
    )
    .unwrap();
    seq.activate_protocol_at(engine_core::protocol::ProtocolVersion::V23).unwrap();
    seq.genesis(&genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    while queued.try_recv().is_ok() {}
    (seq, queued)
}

#[test]
fn a_full_posting_queue_is_one_alert_episode_that_resolves_under_its_own_key() {
    let (mut seq, mut queued) = gated("backlog-episode", 1);
    // Fill the queue the bootstrap drained, then defer at several sequences.
    seq.sequence(Command::Tick).unwrap();
    let lines = crate::alerts::capture(|| {
        for _ in 0..3 {
            seq.sequence(Command::Tick).unwrap();
        }
        queued.try_recv().unwrap();
        seq.sequence(Command::Tick).unwrap();
    });
    let key = "checkpoint_backlog{}".to_owned();
    assert_eq!(
        crate::alerts::pages_for(lines),
        [(crate::alerts::Status::Firing, key.clone()), (crate::alerts::Status::Resolved, key)]
    );
}

#[test]
fn a_custody_prefix_hold_is_one_alert_episode_that_resolves_when_signing_resumes() {
    let (mut seq, mut queued) = gated("custody-prefix-withheld", 128);
    let lines = crate::alerts::capture(|| {
        credit(&mut seq);
        for _ in 0..3 {
            seq.sequence(Command::Tick).unwrap();
        }
        assert!(queued.try_recv().is_err(), "the hold withholds every attempt");
        finalise(&mut seq);
    });
    let withheld = lines.iter().filter(|line| line.name == "checkpoint_withheld" && line.severity.is_some()).count();
    assert!(withheld >= 4, "one line per attempt at different sequences: {lines:?}");
    let key = "checkpoint_withheld{cause=custody_prefix}".to_owned();
    assert_eq!(
        crate::alerts::pages_for(lines),
        [(crate::alerts::Status::Firing, key.clone()), (crate::alerts::Status::Resolved, key)]
    );
    assert_eq!(drain(&mut queued).coverage, DepositCoverage::Canonical);
}

fn receipt() -> DepositReceipt {
    DepositReceipt {
        block: 1,
        block_hash: B256::repeat_byte(1),
        tx_hash: B256::repeat_byte(2),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: DepositBeneficiary::Trader { account: crate::test_support::TRADER },
        amount: Fixed::from_int(10),
    }
}

fn credit(seq: &mut Sequencer) {
    seq.sequence(Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: receipt() }))).unwrap();
    assert!(seq.engine.recorded_deposit(&receipt().reference()).is_some());
}

fn finalise(seq: &mut Sequencer) {
    let receipt = receipt();
    let observed_at_seq = seq.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    seq.sequence(Command::CustodyDeposit(Box::new(DepositUpdate::Finalise { receipt, observed_at_seq }))).unwrap();
    seq.sequence(Command::CustodyFinalised(CustodyCheckpoint {
        block: 1,
        block_hash: B256::repeat_byte(1),
        deposit_count: 1,
    }))
    .unwrap();
    assert_eq!(seq.engine.custody_deposit_coverage(), Some(1));
}

fn custody_catchup(seq: &mut Sequencer) {
    credit(seq);
    finalise(seq);
}

fn drain(queued: &mut mpsc::Receiver<Work>) -> Work {
    let mut last = None;
    while let Ok(work) = queued.try_recv() {
        last = Some(work);
    }
    last.expect("a checkpoint should have been published")
}
