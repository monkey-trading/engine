//! Quote read pacing: when a connection's read goes out after a head move, and how many reads
//! the connections share.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::advance;

use super::tests::{INTERVAL, Rig, prices};

#[tokio::test(start_paused = true)]
async fn a_read_after_a_quiet_spell_goes_out_the_moment_the_head_moves() {
    let mut rig = Rig::new();
    rig.subscribe("[1]");
    rig.within(INTERVAL).await.expect("the first quotes");
    advance(Duration::from_secs(5)).await;
    let moved = tokio::time::Instant::now();
    rig.sequence(prices(101_000));
    let quotes = rig.within(Duration::from_secs(1)).await.expect("the changed quote");
    assert_eq!(quotes[0]["mark"], "101000");
    assert_eq!(moved.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn a_move_inside_the_interval_is_read_when_the_interval_ends() {
    let mut rig = Rig::new();
    let began = tokio::time::Instant::now();
    rig.subscribe("[1]");
    rig.within(INTERVAL).await.expect("the first quotes");
    advance(Duration::from_millis(40)).await;
    rig.sequence(prices(101_000));
    assert_eq!(rig.within(Duration::from_millis(59)).await, None);
    assert_eq!(rig.queries(), 1);
    assert!(rig.within(Duration::from_millis(2)).await.is_some());
    assert_eq!(began.elapsed(), INTERVAL);
}

#[tokio::test(start_paused = true)]
async fn connections_queue_no_more_reads_than_the_shared_permits_allow() {
    let reads = Arc::new(Semaphore::new(2));
    let mut rigs: Vec<Rig> = (0..3).map(|_| Rig::sharing(reads.clone())).collect();
    for rig in &mut rigs {
        rig.open.send_replace(false);
        rig.subscribe("[1]");
        assert_eq!(rig.within(Duration::from_millis(10)).await, None);
    }
    // Two reads reached their sequencers and hold the permits; the third connection waits its turn.
    let queued: Vec<_> = rigs.iter().map(Rig::queries).collect();
    assert_eq!(queued, [1, 1, 0]);
    // A query that has run gives its permit back, and the waiting connection reads.
    rigs[0].open.send_replace(true);
    rigs[0].within(Duration::from_secs(1)).await.expect("the first connection's quotes");
    rigs[2].open.send_replace(true);
    rigs[2].within(Duration::from_secs(1)).await.expect("the waiting connection's quotes");
    assert_eq!(rigs[2].queries(), 1);
    assert_eq!(reads.available_permits(), 1, "the second connection's read still holds its permit");
}
