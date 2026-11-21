//! How often the poster reads the chain with the v2 heartbeat's inputs while no turn names the operator,
//! and how often it posts heads while one does.

use super::*;

#[tokio::test(start_paused = true)]
async fn with_a_heartbeat_and_no_turn_the_poster_reads_every_30_s_whatever_its_read_interval() {
    let chain = FakeChain::new(None);
    let drive = spawn_retrying(chain.clone(), None, Duration::from_secs(300));
    drive.release(1);
    tokio::time::sleep(Duration::from_secs(301)).await;
    let reads = chain.reads();
    drive.stop().await;
    assert_eq!(reads, 11, "reads at 0 s to 300 s");
}

#[tokio::test(start_paused = true)]
async fn with_a_heartbeat_a_failed_first_read_is_retried_within_30_s_whatever_the_read_interval() {
    let chain = FakeChain::new(None);
    chain.fail_next_reads(1);
    let drive = spawn_retrying(chain.clone(), None, Duration::from_secs(3_600));
    drive.release(1);
    tokio::time::sleep(Duration::from_secs(31)).await;
    let reads = chain.reads();
    drive.stop().await;
    assert_eq!(reads, 2, "the failed read at 0 s, and one at 30 s");
}

/// In a turn, heads go out every half interval: on the third read tick of a 60 s interval's 10 s ticks.
#[tokio::test(start_paused = true)]
async fn in_a_turn_heads_go_out_every_half_interval() {
    let gaps = post_gaps(Duration::ZERO, 18).await;
    assert!(gaps.len() >= 5, "three minutes of heads every 30 s: {gaps:?}");
    assert!(gaps.iter().all(|gap| *gap == Duration::from_secs(30)), "{gaps:?}");
}

/// A turn read slower than half an interval skips the read tick the next post was due at, so the post
/// waits for the tick after it: heads go out two thirds of an interval apart.
#[tokio::test(start_paused = true)]
async fn a_turn_read_slower_than_half_an_interval_spaces_heads_two_thirds_of_one_apart() {
    let gaps = post_gaps(Duration::from_secs(35), 30).await;
    assert!(gaps.len() >= 5, "{gaps:?}");
    // The first gap also carries the read cadence's start at the first read's end.
    assert!(gaps[1..].iter().all(|gap| *gap == Duration::from_secs(40)), "{gaps:?}");
}

// ── Helpers ──

/// The time between consecutive heads over `steps` steps of 10 s, each releasing and confirming a newer
/// seq, with each turn read taking `latency`, a 60 s heartbeat interval and a read interval of a minute,
/// so the heartbeat alone paces the reads.
async fn post_gaps(latency: Duration, steps: u64) -> Vec<Duration> {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    chain.slow_turn_reads(latency);
    let keys: HeldKeys = Arc::new(|| (RELEASE, GENERATION));
    let drive = spawn_retrying(chain.clone(), Some(keys), Duration::from_secs(60));
    for seq in 1..=steps {
        drive.release(seq);
        drive.confirm(seq, 0xc1);
        drive.clock.advance(Duration::from_secs(10));
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    drive.stop().await;
    chain.posted_v2_at().windows(2).map(|pair| pair[1] - pair[0]).collect()
}
