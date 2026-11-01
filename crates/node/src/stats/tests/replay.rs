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
}

/// What the tests below do to a rig: a trade and its round trip, so volume, levels and the activity
/// all hold something.
fn script(rig: &mut Rig) {
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 2);
    rig.price(T0 + 1_500, 99_000);
    rig.trade(T0 + 2_000, TRADER, Side::Sell, 2);
    rig.apply(T0 + 600_000, Command::Tick);
}

/// A recovery attempt that fails part way and is retried must not count a command twice: after a
/// restart the same record, replayed again, holds exactly what one clean replay holds.
#[test]
fn a_restarted_record_replayed_again_holds_what_one_replay_holds() {
    let mut first = Rig::new();
    script(&mut first);
    let attempt = first.stats.clone();
    let once = {
        let mut clean = Rig::new();
        script(&mut clean);
        clean.stats.dump()
    };
    assert_eq!(attempt.dump(), once, "the script is deterministic");
    assert!(
        attempt.dump().contains("Volume") && attempt.dump().contains("LossClose"),
        "the record holds flows and activity"
    );
    attempt.restart();
    assert_ne!(attempt.dump(), once, "a restart empties the record");
    let mut again = Rig::recording_into(attempt.clone());
    script(&mut again);
    assert_eq!(attempt.dump(), once);
}
