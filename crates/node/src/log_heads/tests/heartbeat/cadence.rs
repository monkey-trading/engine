//! How often the poster reads the chain with the v2 heartbeat's inputs while no turn names the operator,
//! and how often it posts heads while one does.

use super::*;

/// The tightest rule the schedule accepts at 49 s slots (`HeartbeatPaceTightestTest`): a turn's first head is
/// due within 24.5 s of its boundary.
const TIGHTEST_INTERVAL: Duration = Duration::from_secs(49);

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
        for retry in
            [Duration::from_millis(7), Duration::from_millis(99), secs(1), secs(5), secs(7), secs(30), secs(3_600)]
        {
            let (period, pace) = crate::log_heads::v2::cadence(interval, retry);
            assert!(period <= retry.min(interval / 6) && !period.is_zero(), "{interval:?}, {retry:?}: {period:?}");
            assert!(pace <= interval / 2, "{interval:?}, {retry:?}: {pace:?}");
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

/// A poster started on an empty log reads the chain at the log's first release, not at the read tick
/// after it: in a turn, its first head goes out as soon as a member confirms the log, not a 30 s read
/// interval later.
#[tokio::test(start_paused = true)]
async fn on_an_empty_log_the_first_read_comes_at_the_first_release() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    let keys: HeldKeys = Arc::new(|| (RELEASE, GENERATION));
    let drive = spawn_retrying(chain.clone(), Some(keys), Duration::from_secs(30));
    // The log's genesis is released, and a member confirms it, a second after the poster starts.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(chain.reads(), 0, "nothing to read for before the first release");
    drive.release(1);
    drive.confirm(1, 0xc1);
    drive.clock.advance(Duration::from_secs(1));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let posted = chain.posts_v2();
    drive.stop().await;
    assert_eq!(posted.iter().map(|(_, head)| head.head).collect::<Vec<_>>(), vec![at(1)], "the first head");
}

/// At the first turn after none named the operator, the poster reads the chain from the turn's boundary as
/// trusted time places it, whatever the phase of its 30 s reads and the host clock's offset, and sends the
/// turn's first head at the first read once the agreed block names the turn and a member has confirmed the
/// log, though it sent a v1 head just before: within half an interval of the boundary under the tightest rule
/// at 49 s slots, the block or the confirmation a few seconds late or not. Its reads from the boundary end
/// there, and the next head goes out half an interval after the first.
#[tokio::test(start_paused = true)]
async fn a_new_turns_first_head_goes_out_from_its_boundary_within_half_an_interval() {
    let boundary = engine_core::turns::boundary(turn(GENERATION).turn).expect("the turn has a boundary");
    // Seconds from the poster's start, its first read and v1 head, to the boundary; from the boundary to the
    // agreed block that names the turn; from that to a member's confirmation; and the host clock's lag.
    let cases: [(i64, u64, u64, i64); 7] =
        [(1, 0, 0, 0), (6, 0, 0, 0), (14, 0, 3, 0), (23, 0, 0, 0), (29, 0, 0, 0), (12, 5, 2, 0), (20, 0, 0, 6)];
    for (before, lag, confirm, behind) in cases {
        let case = format!("{before} s before, {lag} s late, confirmed {confirm} s on, {behind} s behind");
        let chain = FakeChain::new(None);
        let keys: HeldKeys = Arc::new(|| (RELEASE, GENERATION));
        let drive = spawn_retrying_at(chain.clone(), Some(keys), Duration::from_secs(30), boundary - before * 1000);
        drive.clock.skew(-behind * 1000);
        // As the liveness rule lets it sign while no turn names the operator: at the tip, up to the boundary.
        drive.signing.send_replace(Signing::Tip(None, boundary.unsigned_abs()));
        drive.release(1);
        pass(&drive, before.unsigned_abs() + lag).await;
        assert_eq!(chain.posts().len(), 1, "{case}: the v1 head before the turn");
        chain.name_turn(turn(GENERATION));
        chain.change_turn(|turn| turn.interval = TIGHTEST_INTERVAL);
        drive.release(2);
        pass(&drive, confirm).await;
        drive.confirm(2, 0xc1);
        let confirmed = tokio::time::Instant::now();
        pass(&drive, 2).await;
        let first = chain.posted_v2_at().first().copied().unwrap_or_else(|| panic!("{case}: no head"));
        assert!(first - confirmed <= Duration::from_secs(1), "{case}: {:?} after the confirmation", first - confirmed);
        assert!(first - confirmed + Duration::from_secs(lag + confirm) <= TIGHTEST_INTERVAL / 2, "{case}");
        drive.release(3);
        drive.confirm(3, 0xc1);
        let turn_reads = chain.turn_reads();
        pass(&drive, 9).await;
        let turn_reads = chain.turn_reads() - turn_reads;
        pass(&drive, 20).await;
        let second = chain.posted_v2_at().get(1).map(|at| *at - first);
        drive.stop().await;
        assert!(turn_reads <= 1, "{case}: {turn_reads} turn reads in the 9 s after the first head");
        let pace = crate::log_heads::v2::cadence(TIGHTEST_INTERVAL, Duration::from_secs(30)).1;
        assert_eq!(second, Some(pace), "{case}: the second head half an interval after the first");
    }
}

/// A turn's first head that the relayer holds back, sending nothing behind a send still pending at its nonce,
/// goes out at the boundary's next read, a second later, not half an interval later.
#[tokio::test(start_paused = true)]
async fn a_first_head_the_relayer_holds_back_goes_out_at_the_next_read_from_the_boundary() {
    let boundary = engine_core::turns::boundary(turn(GENERATION).turn).expect("the turn has a boundary");
    let chain = FakeChain::new(None);
    let keys: HeldKeys = Arc::new(|| (RELEASE, GENERATION));
    let drive = spawn_retrying_at(chain.clone(), Some(keys), Duration::from_secs(30), boundary - 10_000);
    drive.signing.send_replace(Signing::Tip(None, boundary.unsigned_abs()));
    drive.release(1);
    pass(&drive, 10).await;
    chain.name_turn(turn(GENERATION));
    chain.change_turn(|turn| turn.interval = TIGHTEST_INTERVAL);
    chain.wait_next_v2_posts(1);
    drive.release(2);
    drive.confirm(2, 0xc1);
    let named = tokio::time::Instant::now();
    pass(&drive, 3).await;
    let first = chain.posted_v2_at().first().map(|at| *at - named);
    drive.stop().await;
    assert!(first.is_some_and(|first| first <= Duration::from_secs(2)), "held back once, then sent: {first:?}");
}

/// A turn's first head that the relayer holds back on a read tick, not a read from the boundary, as for a node
/// that finds the turn it holds at its start, goes out at the next read tick, not half an interval later.
#[tokio::test(start_paused = true)]
async fn a_first_head_held_back_on_a_read_tick_goes_out_at_the_next_tick() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    chain.change_turn(|turn| turn.interval = TIGHTEST_INTERVAL);
    chain.wait_next_v2_posts(1);
    let keys: HeldKeys = Arc::new(|| (RELEASE, GENERATION));
    let started = tokio::time::Instant::now();
    let drive = spawn_retrying(chain.clone(), Some(keys), Duration::from_secs(30));
    drive.release(1);
    drive.confirm(1, 0xc1);
    pass(&drive, 10).await;
    let first = chain.posted_v2_at().first().map(|at| *at - started);
    drive.stop().await;
    let tick = crate::log_heads::v2::cadence(TIGHTEST_INTERVAL, Duration::from_secs(30)).0;
    assert_eq!(first, Some(tick), "held back at the start's read, sent at the next tick");
}

/// Where no turn names the operator from a boundary, the poster reads the chain every second from when true
/// time may have reached the boundary until it has certainly passed it by 30 s, the 30 s turn reads keeping
/// their cadence, then every 30 s again; on monotonic time, so a host clock held at the boundary changes
/// nothing.
#[tokio::test(start_paused = true)]
async fn a_boundary_that_names_no_turn_is_read_for_30_s_then_every_30_s() {
    let boundary = engine_core::turns::boundary(turn(GENERATION).turn).expect("the turn has a boundary");
    for stalled in [false, true] {
        let chain = FakeChain::new(None);
        chain.slow_turn_reads(Duration::from_millis(100));
        let drive = spawn_retrying_at(chain.clone(), None, Duration::from_secs(30), boundary - 10_000);
        drive.release(1);
        // Reads to 1 s after the start, then to 40 s past the boundary, then to 130 s past it.
        let mut counts = Vec::new();
        let mut second = 0;
        for until in [1, 50, 140] {
            while second < until {
                second += 1;
                drive.clock.advance(Duration::from_secs(1));
                if stalled && second > 10 {
                    // The host clock stands at the boundary, its sample's monotonic time moving on.
                    drive.clock.skew(-(second - 10) * 1000);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            counts.push((chain.reads(), chain.turn_reads()));
        }
        drive.stop().await;
        let [(start_reads, start_turns), (search_reads, search_turns), (end_reads, end_turns)] = counts[..] else {
            unreachable!("three windows")
        };
        assert_eq!((start_reads, start_turns), (1, 1), "stalled {stalled}: the start's read");
        // The sample allows 3 s either side of true time: a read a second for 36 s, and the 30 s read.
        let searched = (search_reads - start_reads, search_turns - start_turns);
        assert!((35..=39).contains(&searched.0) && searched.0 == searched.1, "stalled {stalled}: {searched:?}");
        let after = (end_reads - search_reads, end_turns - search_turns);
        assert_eq!(after, (3, 3), "stalled {stalled}: a read and a turn read every 30 s once the search ends");
    }
}

// ── Helpers ──

/// Moves the poster's clock and the runtime's on by `secs` seconds, a second at a time.
async fn pass(drive: &Drive, secs: u64) {
    for _ in 0..secs {
        drive.clock.advance(Duration::from_secs(1));
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

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
