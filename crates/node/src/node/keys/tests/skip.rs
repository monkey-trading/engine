//! The release watch under the latest agreed block's skip (#596): keys sealed in a skip's generation
//! page nothing before the finalised schedule shows the skip, a failed read of the latest block is
//! retried rather than judged by the finalised schedule alone, and a latest block naming another
//! holding pages past the turn's grace.

use super::*;
use std::sync::atomic::Ordering;

use crate::checkpoints::ScheduledTurn;
use crate::handover::assigned::Assignments;
use crate::handover::assigned::tests::{FakeAssignments, skip};

#[tokio::test(start_paused = true)]
async fn a_skip_naming_the_sealed_keys_pages_nothing_and_a_failed_latest_read_is_retried() {
    let (_guard, mut alerts) = capture_alerts();
    let sealed = Release { launch_digest: DIGEST, config_sha256: [0xc1; 32] };
    let turn = 20_754;
    let boundary = engine_core::turns::boundary(turn).unwrap();
    // The finalised vault and schedule name the drawn member's holding; the latest block's skip names this one.
    let vault = Vault::naming(CurrentRelease { id: [0x77; 32], generation: 6 });
    *vault.scheduled.lock().unwrap() = Some(ScheduledTurn {
        turn,
        boundary,
        candidate: AccountId([0xb0; 20]),
        admin_key: AccountId([0xb1; 20]),
        release: [0x77; 32],
        generation: 6,
        set: 1,
        source: None,
    });
    let assignments = Arc::new(FakeAssignments::default());
    assignments.set(Some(skip(turn, 1, 0xc1, sealed.id(), 7)));
    let every = Duration::from_secs(60);
    let past = boundary + TURN_HANDOVER_GRACE_MS;
    let sample = crate::trusted_time::Sample { at: std::time::Instant::now(), earliest_ms: past, latest_ms: past + 1 };
    let (_samples, samples_rx) = tokio::sync::watch::channel(Some(sample));
    let clock = Arc::new(crate::clock::SystemClock);
    let trusted = crate::trusted_time::TrustedClock::new(clock, samples_rx, Duration::from_secs(1));
    let reader: Arc<dyn Assignments> = assignments.clone();
    let store = sealed_store("keys-watch-skip", sealed, 7);
    let watch = tokio::spawn(watch_release(vault, store, every, trusted, Some(reader)));
    tokio::time::sleep(every * 3).await;
    assert!(alerts.try_recv().is_err(), "the skip's member was paged before the finalised schedule showed the skip");
    // With no quorum at the latest block, the finalised schedule alone does not page.
    assignments.failing.store(true, Ordering::SeqCst);
    tokio::time::sleep(every * 3).await;
    assert!(alerts.try_recv().is_err(), "a failed latest read paged");
    // A later skip names another holding: past the grace, the keys left here page.
    assignments.failing.store(false, Ordering::SeqCst);
    assignments.set(Some(skip(turn, 2, 0xd1, [0x88; 32], 8)));
    tokio::time::sleep(every).await;
    let alert = alerts.try_recv().expect("keys a later skip moved past raise Critical past the grace");
    assert_eq!(alert["alert"].as_str(), "release_superseded");
    watch.abort();
}
