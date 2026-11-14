//! The stall alarm's window: two posting intervals under rotation, three without.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::B256;
use engine_core::types::AccountId;
use tokio::sync::watch;

use super::FakeChain;
use crate::log::CheckpointDomain;
use crate::log_heads::{Head, Poster};

/// The stall alarm's window follows the rotation switch: two posting intervals with it on, three off.
#[test]
fn the_stall_window_is_two_intervals_under_rotation_and_three_without() {
    let anvil = std::fs::read_to_string(crate::test_support::ANVIL_CONFIG).unwrap();
    let off: crate::config::NodeConfig = toml::from_str(&anvil).unwrap();
    let on: crate::config::NodeConfig =
        toml::from_str(&anvil.replace("rotation_enabled = false", "rotation_enabled = true")).unwrap();
    assert_eq!((crate::log_heads::setup::stall_intervals(&off), crate::log_heads::setup::stall_intervals(&on)), (3, 2));
}

#[tokio::test(start_paused = true)]
async fn the_stall_alert_fires_after_the_configured_intervals_and_pages_once() {
    // One stall, every send lost: the alert fires after `stall_intervals` intervals, once.
    for (intervals, fires_by_130s) in [(2, true), (3, false)] {
        let held = Head { seq: 1, link: B256::from([1; 32]) };
        let chain = FakeChain::new(Some(held));
        chain.fail_next_posts(usize::MAX);
        let (_guard, mut router) = crate::alerts::capture_until_dropped();
        let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
        let (tx, rx) = watch::channel(Head::default());
        let poster = Poster {
            chain,
            key: Arc::new(crate::tee::EnclaveKey::test_key()),
            domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
            interval: Duration::from_secs(60),
            retry: Duration::from_secs(1),
            freeze: crate::log_heads::FreezeLatch::default(),
            confirmed: Some((1, held)),
            anchored: None,
            trusted: None,
            prefix_ring: Default::default(),
            stall_intervals: intervals,
        };
        let task = tokio::spawn(poster.run(rx, sequencer));
        tx.send_replace(Head { seq: 5, link: B256::from([5; 32]) });
        tokio::time::sleep(Duration::from_secs(130)).await;
        let stalls = |lines: &[crate::alerts::AlertLine]| {
            lines.iter().filter(|line| line.name == "log_head_stalled" && line.severity.is_some()).count()
        };
        let lines = router.take();
        assert_eq!(stalls(&lines), usize::from(fires_by_130s), "{intervals} intervals at 130 s: {lines:?}");
        tokio::time::sleep(Duration::from_secs(120)).await;
        let lines = router.take();
        assert_eq!(stalls(&lines), usize::from(!fires_by_130s), "one page per episode ({intervals}): {lines:?}");
        drop(tx);
        assert!(task.await.unwrap().is_ok());
    }
}
