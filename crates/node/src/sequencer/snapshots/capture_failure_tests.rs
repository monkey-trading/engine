//! A capture whose log sync fails, or that fails on its own with no catalog to seal, keeps its
//! checkpoint, as a refused one does. The steps after the seal, and the shutdown capture handing the
//! kept checkpoint to the history owner, are the node's `cadence` tests.

use std::sync::Arc;
use std::time::Duration;

use engine_core::Fixed;
use tokio::sync::{broadcast, mpsc, watch};

use crate::clock::test_clock::TestClock;
use crate::log::Manifest;
use crate::sequencer::{Sequencer, SequencerConfig};
use crate::snapshot::head::StoreId;
use crate::snapshot::sealing::{CaptureStage, Endpoint};
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, sample_around, scratch_dir, sequencer_config};

#[test]
fn a_capture_whose_log_sync_fails_keeps_its_checkpoint() {
    {
        let stage = CaptureStage::Sync;
        let dir = scratch_dir("capture-failure-sync");
        let clock = Arc::new(TestClock::at(1_792_504_800_000));
        let (sink, mut queued) = mpsc::channel(128);
        let (owner, mut taken) = mpsc::channel(8);
        let endpoint = Endpoint::new(owner, StoreId([0x42; 32]), Duration::ZERO);
        let cfg = SequencerConfig {
            clock: clock.clone(),
            checkpoint_every: 1,
            checkpoints: Some(sink),
            history_sealing: Some(endpoint.clone()),
            ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
        };
        let key = Arc::new(EnclaveKey::test_key());
        let mut seq = Sequencer::recover(cfg, key, broadcast::channel(32).0, mpsc::unbounded_channel().0).unwrap();
        seq.activate_legacy_protocol().unwrap();
        seq.genesis(&dev_config().genesis).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        seq.sequence(crate::test_support::empty_custody_frontier()).unwrap();
        while queued.try_recv().is_ok() {}
        // A reconciled checkpoint gives captures their recovery base.
        let base = seq.last_seq().unwrap();
        seq.checkpoint_status(Some(base), Some(crate::checkpoints::Health::Healthy)).unwrap();
        while taken.try_recv().is_ok() {}

        // The next checkpoint's capture fails at `stage`.
        endpoint.fail_next(stage);
        let before = seq.last_checkpoint;
        let receipt = engine_core::deposits::DepositReceipt {
            block: 1,
            block_hash: engine_core::eip712::B256::repeat_byte(1),
            tx_hash: engine_core::eip712::B256::repeat_byte(7),
            log_index: 0,
            transaction_deposit_index: 0,
            deposit_id: 0,
            beneficiary: engine_core::deposits::DepositBeneficiary::House,
            amount: Fixed::from_int(7),
        };
        let command =
            engine_core::command::Command::CustodyDeposit(Box::new(engine_core::deposits::DepositUpdate::Credit {
                receipt,
            }));
        let outcome = seq.stamp(seq.last_ts, command).and_then(|_| seq.flush());
        // A failed log sync fails closed.
        assert!(outcome.is_err(), "{stage:?}: {outcome:?}");
        assert!(taken.try_recv().is_err(), "{stage:?}: a failed capture hands the history owner nothing");
        let kept = seq.captures.uncaptured.take().unwrap_or_else(|| panic!("{stage:?}: the checkpoint is kept"));
        let checkpoint = kept.checkpoint.seq;
        assert!(Some(checkpoint) > before, "{stage:?}: the kept checkpoint is the one the command wrote");

        // This log has no catalog owner, so the next capture of it fails in the seal on its own: the
        // checkpoint is kept again.
        seq.capture(kept).unwrap();
        assert!(taken.try_recv().is_err(), "the capture failed in the seal");
        let kept = seq.captures.uncaptured.take().expect("a capture failing in the seal keeps its checkpoint");
        assert_eq!(kept.checkpoint.seq, checkpoint);
    }
}
