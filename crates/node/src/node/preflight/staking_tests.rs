//! Startup refuses a log or a genesis whose staking epoch or cooldown differs from the staking
//! contract's, and a contract that cannot be read, with a Critical alert for each, and reads nothing
//! when there is nothing to compare.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use engine_core::Fixed;
use engine_core::staking::StakingConfig;

use crate::custody::StakingTerms;
use crate::node::preflight::staking_preflight;
use crate::test_support::{Alert, capture_alerts};

const DAY_MS: i64 = 86_400_000;

fn config(epoch_ms: i64, cooldown_ms: i64) -> StakingConfig {
    StakingConfig { liquidity_floor: Fixed::from_int(30_500_000), epoch_ms, start_ms: 0, cooldown_ms }
}

fn terms(held: Option<(u64, u64)>) -> (StakingTerms, Arc<AtomicUsize>) {
    let reads = Arc::new(AtomicUsize::new(0));
    (StakingTerms::counting(held, reads.clone()), reads)
}

fn alert_of(alerts: &mut tokio::sync::mpsc::UnboundedReceiver<Alert>) -> (String, String) {
    let alert = alerts.try_recv().expect("a refusal raises Critical");
    (alert["alert"].clone(), alert["level"].clone())
}

#[tokio::test]
async fn startup_refuses_a_staking_config_the_contract_does_not_hold_and_an_unreadable_contract() {
    let (_guard, mut alerts) = capture_alerts();
    let right = config(DAY_MS, 28 * DAY_MS);
    let (held, reads) = terms(Some((86_400, 2_419_200)));

    // Nothing to compare with, or nothing to compare: no read, no alert.
    staking_preflight(None, Some(&config(1, 1)), Some(&config(1, 1))).await.unwrap();
    staking_preflight(Some(&held), None, None).await.unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    // The contract's own values, in the log and in the genesis, pass.
    staking_preflight(Some(&held), Some(&right), Some(&right)).await.unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert!(alerts.try_recv().is_err(), "matching values raise nothing");

    // A log whose cooldown differs, and a genesis whose epoch differs, each name where it was found.
    let log = staking_preflight(Some(&held), Some(&config(DAY_MS, DAY_MS)), None).await.unwrap_err();
    assert!(log.to_string().starts_with("the log: cooldown 86400000 ms differs"), "{log:#}");
    assert_eq!(alert_of(&mut alerts), ("staking_terms_mismatch".into(), "ERROR".into()));
    let genesis =
        staking_preflight(Some(&held), Some(&right), Some(&config(1_800_000, 28 * DAY_MS))).await.unwrap_err();
    assert!(genesis.to_string().starts_with("the config's genesis: epoch 1800000 ms differs"), "{genesis:#}");
    assert_eq!(alert_of(&mut alerts), ("staking_terms_mismatch".into(), "ERROR".into()));

    // A contract nobody could read proves nothing matches.
    let (unread, _) = terms(None);
    assert!(staking_preflight(Some(&unread), Some(&right), None).await.is_err());
    assert_eq!(alert_of(&mut alerts), ("checkpoint_startup_failed".into(), "ERROR".into()));
}
