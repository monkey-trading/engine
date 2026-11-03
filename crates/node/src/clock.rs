//! The node's clocks. Inside a TEE the host supplies the wall clock, so the sequencer only uses it
//! to stamp commands, checks it against trusted time, never moves sequenced time backwards, and
//! keeps it moving with monotonic time.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use engine_core::types::Millis;
use tracing::{info, warn};

/// How long the pace of sequenced time runs from one anchor before it may give way.
const PACE_WINDOW: Duration = Duration::from_secs(1);
/// How far behind monotonic time sequenced time may give way per `PACE_WINDOW`: 1%, so a host
/// clock's step back drains rather than holding stamps ahead of it for good. A slew moves the
/// monotonic clock with it, so opens no gap.
const PACE_GIVE_MS: Millis = 10;
/// How far behind the pace the host clock is before that is alerted: past any clock correction's
/// step.
const PACE_ALERT_MS: Millis = 1_000;

/// The clocks the sequencer and the trusted time check read.
pub trait Clock: Send + Sync {
    /// Wall-clock milliseconds since the epoch, as the host reports them.
    fn now_ms(&self) -> Millis;

    /// Monotonic time, for round trips, the age of a trusted time sample, the pace of sequenced
    /// time, and a venue connection's delay bound; the feeds read the same clock through tokio.
    ///
    /// Every bound on a price's age rests on its rate, which the host cannot scale in the enclave
    /// image: it launches with Secure TSC, which a verifier requires of a release, and a node with
    /// real attestation starts only once the kernel times on that TSC (`tee::require_tsc_clocksource`).
    fn instant(&self) -> Instant;
}

/// The operating system's clocks.
pub struct SystemClock;

/// The least the next sequenced stamp may be: sequenced time keeps pace with monotonic time
/// whatever the host clock does, giving up at most `PACE_GIVE_MS` of it a `PACE_WINDOW`, so a quote
/// the engine holds ages out within about its `max_age_ms` of arriving even if the host clock
/// stalls or steps back. It gives way that much so a monotonic clock running a little fast, or a
/// host clock's correction back, does not ratchet sequenced time ahead for good. A resumed run's
/// stamps also hold at its start until the pace from its last stamp reaches it, without the pace
/// running on from there (`anchored`); a pace with no anchor runs on from its first stamp.
#[derive(Debug)]
pub struct StampPace {
    /// The least any stamp of the run may be.
    start: Millis,
    /// A stamp and the monotonic time it was given, which the pace moves on from.
    anchor: Option<(Millis, Instant)>,
    /// Whether the host clock is alerted as having fallen behind.
    behind: bool,
}

impl Clock for SystemClock {
    /// Wall-clock milliseconds since the epoch.
    fn now_ms(&self) -> Millis {
        now_ms()
    }

    /// `Instant::now()`.
    fn instant(&self) -> Instant {
        Instant::now()
    }
}

impl StampPace {
    /// A pace whose stamps are at least `start`, moving on from the first.
    pub fn new(start: Millis) -> Self {
        Self { start, anchor: None, behind: false }
    }

    /// This pace, moving on from `ms` at monotonic time `at` rather than from its first stamp.
    pub fn anchored(self, ms: Millis, at: Instant) -> Self {
        Self { anchor: Some((ms, at)), ..self }
    }

    /// The least the stamp given at monotonic time `now` may be.
    pub fn floor(&self, now: Instant) -> Millis {
        self.start.max(self.pace(now))
    }

    /// Whether a stamp given now would be held at the start, past both the host clock reading
    /// `host_ms` and the pace.
    pub fn holding_start(&self, host_ms: Millis, now: Instant) -> bool {
        self.start > host_ms.max(self.pace(now))
    }

    /// Records `ts`, stamped at monotonic time `now` from the host clock reading `host_ms`: a host
    /// clock above the pace moves it up, and once a `PACE_WINDOW` has run the pace gives way towards
    /// the host clock by at most `PACE_GIVE_MS`. A stamp held at the last one or at the start moves
    /// nothing, or the next stamp would undo the give, or the pace would run on from the start.
    /// Warns when the host clock has fallen `PACE_ALERT_MS` behind the pace, and logs when it is
    /// back.
    pub fn stamped(&mut self, host_ms: Millis, ts: Millis, now: Instant) {
        let Some((_, at)) = self.anchor else {
            self.anchor = Some((ts, now));
            return;
        };
        let pace = self.pace(now);
        let behind = pace.saturating_sub(host_ms) >= PACE_ALERT_MS;
        if behind && !self.behind {
            // The gap changes while the clock stays behind, so it is in the message, not the key.
            warn!(
                alert = "host_clock_behind",
                "host clock {}ms behind monotonic time: stamps keep monotonic pace",
                pace.saturating_sub(host_ms)
            );
        } else if self.behind && !behind {
            info!(alert_resolved = "host_clock_behind", "host clock back with monotonic time");
        }
        self.behind = behind;
        if host_ms > pace {
            self.anchor = Some((host_ms, now));
        } else if now.saturating_duration_since(at) >= PACE_WINDOW {
            self.anchor = Some((host_ms.max(pace.saturating_sub(PACE_GIVE_MS)), now));
        }
    }

    /// Where the pace has reached at `now`: the anchor moved on by the monotonic time since.
    fn pace(&self, now: Instant) -> Millis {
        self.anchor.map_or(Millis::MIN, |(ms, at)| {
            let elapsed = Millis::try_from(now.saturating_duration_since(at).as_millis()).unwrap_or(Millis::MAX);
            ms.saturating_add(elapsed)
        })
    }
}

/// Wall-clock milliseconds since the epoch.
pub fn now_ms() -> Millis {
    // The epoch is in the past on any machine that can run the node.
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after epoch").as_millis() as Millis
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trusted_time::tests::logs::Logs;

    const START: Millis = 1_792_530_000_000;
    const STEP: Duration = Duration::from_millis(50);

    /// A host clock's reading, by milliseconds since a run's start.
    type HostClock = fn(Millis) -> Millis;
    /// How far the pace may fall behind monotonic time over `elapsed_ms`: 1%, a `PACE_GIVE_MS` that
    /// has not yet had its window, and a millisecond of rounding.
    fn allowed_ms(elapsed_ms: Millis) -> Millis {
        elapsed_ms / 100 + PACE_GIVE_MS + 1
    }

    /// The stamps a pace gives over two minutes, a stamp each `STEP`, from a host clock reading
    /// `host(ms since the start)`, as `(ms since the start, stamp)`.
    fn run(host: impl Fn(Millis) -> Millis) -> Vec<(Millis, Millis)> {
        run_every(STEP, Duration::from_secs(120), host)
    }

    /// The stamps a pace gives over `span`, a stamp each `step`, as `run` does.
    fn run_every(step: Duration, span: Duration, host: impl Fn(Millis) -> Millis) -> Vec<(Millis, Millis)> {
        let at = Instant::now();
        let mut pace = StampPace::new(Millis::MIN);
        let mut last = Millis::MIN;
        let step_ms = step.as_millis() as Millis;
        (0..span.as_millis() as Millis / step_ms)
            .map(|i| {
                let elapsed = i * step_ms;
                let now = at + Duration::from_millis(elapsed as u64);
                let host_ms = START + host(elapsed);
                let ts = host_ms.max(last).max(pace.floor(now));
                pace.stamped(host_ms, ts, now);
                last = ts;
                (elapsed, ts)
            })
            .collect()
    }

    #[test]
    fn stamps_keep_pace_with_monotonic_time_whatever_the_host_clock_does() {
        let hosts: [(&str, HostClock); 6] = [
            ("stalled", |_| 0),
            ("stalled after a minute", |ms| ms.min(60_000)),
            ("a step back", |ms| if ms < 30_000 { ms } else { ms - 20_000 }),
            ("half speed", |ms| ms / 2),
            ("a saw", |ms| ms - ms % 5_000),
            // A spike between the floor's windows must not leave a stamp the floor then lets stand.
            ("spiked within a window, otherwise far behind", |ms| if ms % 1_000 == 500 { ms + 15_000 } else { 0 }),
        ];
        for (name, host) in hosts {
            let stamps = run(host);
            for (i, (from_ms, from_ts)) in stamps.iter().enumerate() {
                for (to_ms, to_ts) in stamps[i..].iter().step_by(37) {
                    let elapsed = to_ms - from_ms;
                    assert!(
                        to_ts - from_ts >= elapsed - allowed_ms(elapsed),
                        "{name}: from {from_ms}ms to {to_ms}ms, stamps moved {}ms",
                        to_ts - from_ts
                    );
                }
            }
        }
    }

    #[test]
    fn an_honest_host_clock_is_never_overridden() {
        for (elapsed, ts) in run(|ms| ms) {
            assert_eq!(ts, START + elapsed);
        }
    }

    #[test]
    fn a_host_clock_correction_back_drains_at_the_give() {
        // A 300ms step back at ten seconds: stamps hold their pace, then rejoin the host clock.
        let stamps = run(|ms| if ms < 10_000 { ms } else { ms - 300 });
        let ahead = |at: Millis| {
            let (elapsed, ts) = stamps.iter().find(|(elapsed, _)| *elapsed == at).expect("a stamp at that time");
            ts - (START + elapsed - 300)
        };
        assert!(ahead(11_000) >= 280, "{}", ahead(11_000));
        assert!(ahead(25_000) <= 160, "{}", ahead(25_000));
        assert_eq!(ahead(45_000), 0);
    }

    #[test]
    fn a_correction_drains_at_the_give_with_stamps_closer_together_than_it() {
        // Stamps a millisecond apart: each still at the last one must not undo the give.
        let stamps =
            run_every(Duration::from_millis(1), Duration::from_secs(50), |ms| if ms < 10_000 { ms } else { ms - 300 });
        let ahead = |at: Millis| stamps[at as usize].1 - (START + at - 300);
        assert!(ahead(25_000) <= 160, "{}", ahead(25_000));
        assert_eq!(ahead(45_000), 0);
    }

    #[test]
    fn a_host_clock_slow_against_monotonic_time_within_the_give_stays_on_the_host_clock() {
        // Faster than any clock discipline slews.
        let stamps = run(|ms| ms - ms / 200);
        for (elapsed, ts) in stamps {
            assert!(ts - (START + elapsed - elapsed / 200) <= PACE_GIVE_MS, "at {elapsed}ms");
        }
    }

    #[test]
    fn a_run_holds_at_its_start_until_the_pace_reaches_it_without_running_on_from_it() {
        let at = Instant::now();
        let mut pace = StampPace::new(START + 1_000).anchored(START, at);
        // The host clock stands where the last run stopped: stamps hold at the start.
        for ms in [0, 500, 999] {
            let now = at + Duration::from_millis(ms);
            assert!(pace.holding_start(START, now), "at {ms}ms");
            assert_eq!(pace.floor(now), START + 1_000);
            pace.stamped(START, START + 1_000, now);
        }
        // Once the pace from the last stamp passes the start, it carries on from there, not ahead.
        let past = at + Duration::from_millis(1_500);
        assert!(!pace.holding_start(START, past));
        assert_eq!(pace.floor(past), START + 1_500);
    }

    #[test]
    fn the_first_stamp_is_at_least_the_start_and_keeps_pace_from_there() {
        let at = Instant::now();
        let mut pace = StampPace::new(START + 1_000);
        assert_eq!(pace.floor(at), START + 1_000);
        // The host clock reads before the start, so the stamp is the start.
        pace.stamped(START, START + 1_000, at);
        assert_eq!(pace.floor(at + STEP), START + 1_000 + STEP.as_millis() as Millis);
    }

    #[test]
    fn a_host_clock_behind_is_alerted_once_and_resolved() {
        let (logs, _guard) = Logs::capture();
        let at = Instant::now();
        let mut pace = StampPace::new(Millis::MIN);
        pace.stamped(START, START, at);
        // Under the alert's threshold nothing fires.
        let near = at + Duration::from_millis(PACE_ALERT_MS as u64 - 1);
        pace.stamped(START, pace.floor(near), near);
        assert_eq!(logs.warnings("host_clock_behind"), 0);
        // The host clock stands still: past the threshold once, however often it is stamped.
        for ms in [2_000, 3_000, 4_000] {
            let now = at + Duration::from_millis(ms);
            pace.stamped(START, pace.floor(now), now);
        }
        assert_eq!(logs.warnings("host_clock_behind"), 1, "once per episode");
        // Back with monotonic time, it resolves.
        let back = at + Duration::from_secs(5);
        pace.stamped(START + 5_000, pace.floor(back).max(START + 5_000), back);
        assert_eq!(logs.count("alert_resolved=\"host_clock_behind\""), 1);
    }
}

#[cfg(test)]
pub mod test_clock {
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use engine_core::types::Millis;

    use crate::clock::{Clock, now_ms};

    /// A host clock that stands still unless a test moves it. True time is the system's at
    /// creation plus `advance`; the wall clock adds `skew` on top, and monotonic time moves with
    /// `advance`.
    pub struct TestClock {
        wall_ms: Millis,
        instant: Instant,
        skew_ms: AtomicI64,
        advanced_ms: AtomicU64,
    }

    impl Default for TestClock {
        /// A clock at the system's current time, unskewed.
        fn default() -> Self {
            Self {
                wall_ms: now_ms(),
                instant: Instant::now(),
                skew_ms: AtomicI64::new(0),
                advanced_ms: AtomicU64::new(0),
            }
        }
    }

    impl TestClock {
        /// A clock whose true time starts at `wall_ms`.
        pub fn at(wall_ms: Millis) -> Self {
            Self { wall_ms, ..Self::default() }
        }

        /// Sets how far the wall clock runs ahead of true time (behind when negative).
        pub fn skew(&self, ms: Millis) {
            self.skew_ms.store(ms, Ordering::SeqCst);
        }

        /// Moves true time, the wall clock, and monotonic time forward by `by`.
        pub fn advance(&self, by: Duration) {
            self.advanced_ms.fetch_add(by.as_millis() as u64, Ordering::SeqCst);
        }

        /// True time: what an honest Roughtime server reports.
        pub fn true_ms(&self) -> Millis {
            self.wall_ms + self.advanced_ms.load(Ordering::SeqCst) as Millis
        }
    }

    impl Clock for TestClock {
        /// True time plus the skew.
        fn now_ms(&self) -> Millis {
            self.true_ms() + self.skew_ms.load(Ordering::SeqCst)
        }

        /// The creation instant plus the advance.
        fn instant(&self) -> Instant {
            self.instant + Duration::from_millis(self.advanced_ms.load(Ordering::SeqCst))
        }
    }
}
