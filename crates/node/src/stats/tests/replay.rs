use std::time::Duration;

use engine_core::types::AccountId;
use engine_core::types::Side;

use super::*;
use crate::log::CheckpointDomain;
use crate::sequencer::Sequencer;
use crate::tee::EnclaveKey;
use crate::test_support::{TestSequencer, deposit, scratch_dir};

/// The commands a node sequences, replayed into a fresh record as recovery does, leave the record the
/// sequencer built live: the same buckets, activity and beginning.
#[tokio::test]
async fn recovery_rebuilds_the_record_the_sequencer_built() {
    let dir = scratch_dir("stats-replay");
    let node = TestSequencer::start(&dir, 64);
    node.submit(deposit(1_000, "stats-1")).await;
    for _ in 0..4 {
        node.submit(Command::Tick).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let live = node.stats.clone();
    node.stop().await;

    let rebuilt = stats();
    Sequencer::read_log_with_authorities(
        &dir,
        Some(CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) }),
        Some(AccountId(EnclaveKey::test_key().address())),
        None,
        Default::default(),
        |_| {},
        None,
        Some(rebuilt.clone()),
    )
    .unwrap();
    assert_eq!(rebuilt.dump(), live.dump());
    assert!(live.dump().contains("Nav"), "the record holds the NAV series");
    let _ = Side::Buy;
}

/// A recovery attempt that fails part way and is retried must not count a command twice.
#[test]
fn a_restarted_record_forgets_what_a_failed_attempt_recorded() {
    let mut rig = Rig::new();
    rig.price(T0 + 1_000, 101_000);
    let once = rig.stats.dump();
    rig.stats.restart();
    assert_ne!(rig.stats.dump(), once);
    let mut again = Rig::new();
    again.price(T0 + 1_000, 101_000);
    assert_eq!(again.stats.dump(), once);
}
