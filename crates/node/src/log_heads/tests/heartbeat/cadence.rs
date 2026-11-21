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
    let gaps = post_gaps(Duration::from_secs(60), Duration::from_secs(60), Duration::ZERO, 18).await;
    assert!(gaps.len() >= 5, "three minutes of heads every 30 s: {gaps:?}");
    assert!(gaps.iter().all(|gap| *gap == Duration::from_secs(30)), "{gaps:?}");
}

/// Half an interval falls on a read tick whatever the heartbeat and read intervals: a 50 s interval, whose
/// sixth is no whole number of nanoseconds, and a 7 s read interval, which does not divide 30 s.
#[tokio::test(start_paused = true)]
async fn heads_go_out_every_half_interval_when_neither_interval_divides_it() {
    for (interval, retry, half) in [(50, 60, 25), (60, 7, 30)] {
        let (interval, retry) = (Duration::from_secs(interval), Duration::from_secs(retry));
        let gaps = post_gaps(interval, retry, Duration::ZERO, 18).await;
        assert!(gaps.len() >= 5, "{interval:?} and {retry:?}: {gaps:?}");
        assert!(gaps.iter().all(|gap| *gap == Duration::from_secs(half)), "{interval:?} and {retry:?}: {gaps:?}");
    }
}

/// The read period is at most a sixth of the interval and the read interval, and a whole number of
/// periods makes half the interval, to the nanosecond the division leaves.
#[test]
fn a_whole_number_of_read_periods_makes_half_an_interval() {
    let secs = Duration::from_secs;
    for interval in [secs(30), secs(49), secs(50), secs(60), secs(140), Duration::from_millis(31_337)] {
        for retry in [secs(1), secs(5), secs(7), secs(30), secs(3_600)] {
            let (period, pace) = crate::log_heads::v2::cadence(interval, retry);
            assert!(period <= retry.min(interval / 6) && !period.is_zero(), "{interval:?}, {retry:?}: {period:?}");
            assert_eq!(pace.as_nanos() % period.as_nanos(), 0, "{interval:?}, {retry:?}");
            let short = (interval / 2).as_nanos() - pace.as_nanos();
            assert!(short < pace.as_nanos() / period.as_nanos(), "{interval:?}, {retry:?}: {pace:?}");
        }
    }
}

/// A turn read slower than half an interval skips the read tick the next post was due at, so the post
/// waits for the tick after it: heads go out two thirds of an interval apart.
#[tokio::test(start_paused = true)]
async fn a_turn_read_slower_than_half_an_interval_spaces_heads_two_thirds_of_one_apart() {
    let gaps = post_gaps(Duration::from_secs(60), Duration::from_secs(60), Duration::from_secs(35), 30).await;
    assert!(gaps.len() >= 5, "{gaps:?}");
    // The first gap also carries the read cadence's start at the first read's end.
    assert!(gaps[1..].iter().all(|gap| *gap == Duration::from_secs(40)), "{gaps:?}");
}

/// In a turn the stall alarm's two posting intervals make one heartbeat interval: with every head refused,
/// it fires once the head on chain has not advanced for more than 60 s, and not before.
#[tokio::test(start_paused = true)]
async fn in_a_turn_the_stall_alert_fires_after_one_heartbeat_interval() {
    let (_guard, mut router) = crate::alerts::capture_until_dropped();
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    chain.fail_v2_posts();
    let keys: HeldKeys = Arc::new(|| (RELEASE, GENERATION));
    let drive = spawn_retrying(chain.clone(), Some(keys), Duration::from_secs(60));
    let fired = |lines: &[crate::alerts::AlertLine]| {
        lines.iter().filter(|line| line.name == "log_head_stalled" && line.severity.is_some()).count()
    };
    steps(&drive, 1..=6).await;
    assert_eq!(fired(&router.take()), 0, "fired within a heartbeat interval");
    steps(&drive, 7..=8).await;
    assert_eq!(fired(&router.take()), 1, "past a heartbeat interval");
    drive.stop().await;
}

// ── Helpers ──

/// The time between consecutive heads over `count` steps of 10 s, each releasing and confirming a newer
/// seq, with each turn read taking `latency`, under a heartbeat `interval` and a read interval `retry`.
async fn post_gaps(interval: Duration, retry: Duration, latency: Duration, count: u64) -> Vec<Duration> {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    chain.change_turn(|turn| turn.interval = interval);
    chain.slow_turn_reads(latency);
    let keys: HeldKeys = Arc::new(|| (RELEASE, GENERATION));
    let drive = spawn_retrying(chain.clone(), Some(keys), retry);
    steps(&drive, 1..=count).await;
    drive.stop().await;
    chain.posted_v2_at().windows(2).map(|pair| pair[1] - pair[0]).collect()
}

/// Releases and confirms each of `seqs` in turn, 10 s apart.
async fn steps(drive: &Drive, seqs: std::ops::RangeInclusive<u64>) {
    for seq in seqs {
        drive.release(seq);
        drive.confirm(seq, 0xc1);
        drive.clock.advance(Duration::from_secs(10));
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
