//! How often the poster reads the chain with the v2 heartbeat's inputs while no turn names the operator.

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

#[tokio::test(start_paused = true)]
async fn in_a_turn_slow_turn_reads_still_let_heads_out_every_third_of_the_interval() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    chain.slow_turn_reads(Duration::from_secs(1));
    let keys: HeldKeys = Arc::new(|| (RELEASE, GENERATION));
    // A read interval of a minute, so the heartbeat alone paces the reads: every 10 s of a 60 s interval.
    let drive = spawn_retrying(chain.clone(), Some(keys), Duration::from_secs(60));
    for seq in 1..=18 {
        drive.release(seq);
        drive.confirm(seq, 0xc1);
        drive.clock.advance(Duration::from_secs(10));
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    let posted = chain.posts_v2().len();
    drive.stop().await;
    assert!(posted >= 9, "three minutes of heads every 20 s, not every 30 s: {posted}");
}
