//! The gate the sequencer checks each stamp against.

use super::*;

const MAX_DRIFT: Duration = Duration::from_secs(10);
/// True time when the sample is taken.
const TRUE_MS: Millis = 1_792_502_815_000;
const SECOND: Duration = Duration::from_secs(1);

#[test]
fn before_the_first_sample_nothing_passes_and_the_alert_waits_for_the_grace() {
    let (logs, _guard) = Logs::capture();
    let start = Instant::now();
    let mut gate = TimeGate::new(MAX_DRIFT, start);
    assert!(!gate.admit(TRUE_MS, TRUE_MS, start));
    assert!(!gate.admit(TRUE_MS, TRUE_MS, start + NO_SAMPLE_GRACE));
    assert_eq!(logs.critical("trusted_time_unavailable"), 0);
    assert!(!gate.admit(TRUE_MS, TRUE_MS, start + NO_SAMPLE_GRACE + Duration::from_millis(1)));
    assert!(!gate.admit(TRUE_MS, TRUE_MS, start + 2 * NO_SAMPLE_GRACE));
    assert_eq!(logs.critical("trusted_time_unavailable"), 1);
}

#[test]
fn a_stamp_within_the_bound_of_every_time_the_sample_allows_passes() {
    let (logs, _guard) = Logs::capture();
    let now = Instant::now();
    let mut gate = sampled(now);
    // The sample allows TRUE_MS ± 3s, so a stamp must be within 7s of TRUE_MS.
    assert!(gate.admit(TRUE_MS - 7_000, TRUE_MS - 7_000, now));
    assert!(gate.admit(TRUE_MS + 7_000, TRUE_MS + 7_000, now));
    assert!(!gate.admit(TRUE_MS + 7_001, TRUE_MS + 7_001, now));
    assert!(!gate.admit(TRUE_MS - 7_001, TRUE_MS - 7_001, now));
    assert_eq!(logs.critical("clock_drift"), 1);
}

#[test]
fn a_host_clock_beyond_the_bound_halts_even_when_the_stamp_is_held_within_it() {
    let (logs, _guard) = Logs::capture();
    let now = Instant::now();
    let mut gate = sampled(now);
    // The host clock stepped back an hour; the stamp stays at the last one, within the bound.
    assert!(!gate.admit(TRUE_MS - 3_600_000, TRUE_MS, now));
    assert_eq!(logs.critical("clock_drift"), 1);
}

#[test]
fn a_stamp_held_beyond_the_bound_halts_even_when_the_host_clock_is_within_it() {
    let (logs, _guard) = Logs::capture();
    let now = Instant::now();
    let mut gate = sampled(now);
    // The last stamp, recovered from the log, is ahead of true time by more than the bound.
    assert!(!gate.admit(TRUE_MS, TRUE_MS + 20_000, now));
    assert_eq!(logs.critical("clock_drift"), 1);
}

#[test]
fn the_first_sample_starts_sequencing_and_a_stamp_back_within_the_bound_resumes_it() {
    let (logs, _guard) = Logs::capture();
    let start = Instant::now();
    let mut gate = TimeGate::new(MAX_DRIFT, start);
    assert!(!gate.admit(TRUE_MS, TRUE_MS, start));
    gate.update(sample(start));
    assert!(gate.admit(TRUE_MS, TRUE_MS, start));
    assert_eq!((logs.count("sequencing started"), logs.count("sequencing resumed")), (1, 0));
    assert!(!gate.admit(TRUE_MS + 20_000, TRUE_MS + 20_000, start));
    assert!(gate.admit(TRUE_MS, TRUE_MS, start));
    assert_eq!((logs.count("sequencing started"), logs.count("sequencing resumed")), (1, 1));
}

#[test]
fn the_bound_moves_on_with_monotonic_time_since_the_sample() {
    let now = Instant::now();
    let mut gate = sampled(now);
    let later = now + 30 * SECOND;
    assert!(gate.admit(TRUE_MS + 30_000, TRUE_MS + 30_000, later));
    assert!(!gate.admit(TRUE_MS, TRUE_MS, later));
}

#[test]
fn the_upper_bound_moves_on_by_the_elapsed_time_rounded_up() {
    let now = Instant::now();
    let mut gate = sampled(now);
    // 30.0005s on, the latest true time is TRUE_MS + 3s + 30.001s, so the lowest stamp allowed is
    // that less the 10s bound.
    let later = now + 30 * SECOND + Duration::from_micros(500);
    assert!(!gate.admit(TRUE_MS + 23_000, TRUE_MS + 23_000, later));
    assert!(gate.admit(TRUE_MS + 23_001, TRUE_MS + 23_001, later));
}

#[test]
fn a_sample_older_than_the_grace_halts_even_a_stamp_within_its_bound() {
    let (logs, _guard) = Logs::capture();
    let now = Instant::now();
    let mut gate = sampled(now);
    assert!(gate.admit(TRUE_MS + 60_000, TRUE_MS + 60_000, now + NO_SAMPLE_GRACE));
    assert!(!gate.admit(TRUE_MS + 61_000, TRUE_MS + 61_000, now + 61 * SECOND));
    assert_eq!(logs.critical("trusted_time_unavailable"), 1);
    assert_eq!(logs.critical("clock_drift"), 0);
}

#[test]
fn sequencing_resumes_once_a_sample_within_the_bound_arrives() {
    let (logs, _guard) = Logs::capture();
    let start = Instant::now();
    let mut gate = TimeGate::new(MAX_DRIFT, start);
    let later = start + 61 * SECOND;
    assert!(!gate.admit(TRUE_MS, TRUE_MS, later));
    assert!(!gate.admit(TRUE_MS, TRUE_MS, later));
    gate.update(sample(later));
    assert!(gate.admit(TRUE_MS, TRUE_MS, later));
    assert_eq!(logs.count("sequencing resumed"), 1);
    assert_eq!(logs.count("refused=2"), 1);
}

#[test]
fn a_halt_alerts_once_and_again_only_when_its_reason_changes() {
    let (logs, _guard) = Logs::capture();
    let now = Instant::now();
    let mut gate = sampled(now);
    // Drift, repeated: one alert.
    (0..3).for_each(|_| assert!(!gate.admit(TRUE_MS + 20_000, TRUE_MS + 20_000, now)));
    assert_eq!(logs.critical("clock_drift"), 1);
    // Still halted, now for want of a sample: a second alert.
    assert!(!gate.admit(TRUE_MS + 61_000, TRUE_MS + 61_000, now + 61 * SECOND));
    assert_eq!(logs.critical("trusted_time_unavailable"), 1);
    // Resumed, then drifting again: a new halt alerts again.
    gate.update(sample(now + 61 * SECOND));
    assert!(gate.admit(TRUE_MS, TRUE_MS, now + 61 * SECOND));
    assert!(!gate.admit(TRUE_MS + 20_000, TRUE_MS + 20_000, now + 61 * SECOND));
    assert_eq!(logs.critical("clock_drift"), 2);
    assert_eq!(logs.count("sequencing resumed"), 1);
}

#[test]
fn the_signing_clock_reads_the_host_clock_only_while_the_latest_sample_bounds_it() {
    let clock = Arc::new(TestClock::at(TRUE_MS));
    let (samples, latest) = watch::channel(None);
    let signing = TrustedClock::new(clock.clone(), latest, MAX_DRIFT);
    assert_eq!(signing.now_ms(), None, "before the first sample");
    samples.send_replace(Some(sample(clock.instant())));
    // The sample allows TRUE_MS ± 3s, so the host clock must be within 7s of TRUE_MS.
    for (skew, read) in [(0, Some(TRUE_MS)), (7_000, Some(TRUE_MS + 7_000)), (-7_000, Some(TRUE_MS - 7_000))] {
        clock.skew(skew);
        assert_eq!(signing.now_ms(), read, "skewed {skew}ms");
    }
    for skew in [7_001, -7_001, 30_000] {
        clock.skew(skew);
        assert_eq!(signing.now_ms(), None, "skewed {skew}ms");
    }
    clock.skew(0);
    clock.advance(NO_SAMPLE_GRACE);
    assert_eq!(signing.now_ms(), Some(clock.now_ms()), "a sample as old as the grace");
    clock.advance(Duration::from_millis(1));
    assert_eq!(signing.now_ms(), None, "a sample older than the grace");
    let true_ms = clock.true_ms();
    samples.send_replace(Some(Sample {
        at: clock.instant(),
        earliest_ms: true_ms - 3_000,
        latest_ms: true_ms + 3_000,
    }));
    assert_eq!(signing.now_ms(), Some(true_ms), "a fresh sample");
}

#[test]
fn the_estimate_moves_the_last_sample_on_by_the_monotonic_clock_past_the_grace() {
    let clock = Arc::new(TestClock::at(TRUE_MS));
    let (samples, latest) = watch::channel(None);
    let estimate = TrustedClock::new(clock.clone(), latest, MAX_DRIFT);
    assert_eq!(estimate.since_sample_ms(), None, "before the first sample");
    samples.send_replace(Some(Sample {
        at: clock.instant(),
        earliest_ms: TRUE_MS - 3_000,
        latest_ms: TRUE_MS + 3_000,
    }));
    // The host's wall clock plays no part, only the time since the sample.
    clock.skew(3_600_000);
    clock.advance(10 * NO_SAMPLE_GRACE);
    assert_eq!(estimate.now_ms(), None, "the sample is past the grace");
    let elapsed = 10 * NO_SAMPLE_GRACE.as_millis() as Millis;
    assert_eq!(estimate.since_sample_ms(), Some(TRUE_MS + 3_000 + elapsed), "the upper bound, moved on");
}

// ── Helpers ──

/// A sample taken at `at`, when true time was `TRUE_MS` ± 3s.
fn sample(at: Instant) -> Sample {
    Sample { at, earliest_ms: TRUE_MS - 3_000, latest_ms: TRUE_MS + 3_000 }
}

/// A gate holding `sample(at)`.
fn sampled(at: Instant) -> TimeGate {
    let mut gate = TimeGate::new(MAX_DRIFT, at);
    gate.update(sample(at));
    gate
}
