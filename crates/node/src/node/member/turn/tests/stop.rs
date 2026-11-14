//! A member's wait stops at a shutdown signal, a replica's build included.

use super::*;

/// A shutdown signal while the member builds its replica, which holds the wait's future while it
/// replays, ends the build at its next entry, and the wait stops without the replica.
#[tokio::test]
async fn a_shutdown_during_the_replicas_build_stops_the_wait() {
    let fixture = Fixture::start("member-stopping", true).await;
    let operator = fixture.operator(true).await;
    fixture.registry.name(&operator.host);
    fixture.time.set(FIRST + 1);
    let mut member = fixture.member();
    // As the wait's watcher sets it once a signal lands, here before the build's first entry.
    member.stopping.store(true, Ordering::Release);
    let turn = tokio::time::timeout(std::time::Duration::from_secs(10), member.until_turn());
    let turn = turn.await.expect("the wait did not stop").unwrap();
    assert!(matches!(turn, Turn::Stop), "the wait went on past the signal");
    assert_eq!(member.metrics.full_replays.get(), 0, "the build replayed the copy past the signal");
}
