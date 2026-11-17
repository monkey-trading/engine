//! The tail check of `price_wait_budget`: a batch's p99 against the runner's own wake-up p99.
//!
//! A slow CI runner alone moves the p99 of unchanged code from about 30 us to 250 us (#879), so no
//! fixed bound can hold both a regression and a slow runner. Each price is therefore timed beside a
//! probe: a thread woken at the same moment, with no engine code behind it. The probe's p99 is the
//! runner's own tail, and a batch's p99 is held to a multiple of it, with a floor for a quiet runner.

/// The most a batch's p99 may be on a quiet runner, in microseconds. Main's batches measure 20 to 37
/// on a normal runner; a tail five to eight times slower (p99 123 to 200 on #870) is over it.
pub(super) const TAIL_FLOOR_US: u128 = 60;
/// How many times the probe's p99 a batch's p99 may be when the runner itself is slow.
pub(super) const TAIL_FACTOR: u128 = 3;
/// Batches that must hold the bound: a batch is lifted alone by a stall of the runner's CPU, as main
/// shows now and then (a p99 of 24 ms in one batch, the rest at 25 us), so two of five may miss it.
pub(super) const TAIL_BATCHES: usize = 3;

/// The p99 a batch may have when its probe's was `probe_p99`.
pub(super) fn allowed_us(probe_p99: u128) -> u128 {
    TAIL_FLOOR_US.max(TAIL_FACTOR.saturating_mul(probe_p99))
}

/// Whether at least `TAIL_BATCHES` of the batches `(p99, probe p99)` hold their bound.
pub(super) fn tail_holds(batches: &[(u128, u128)]) -> bool {
    batches.iter().filter(|(p99, probe)| *p99 <= allowed_us(*probe)).count() >= TAIL_BATCHES
}

/// Diagnostic: runs `round` six times, then fails if any round failed.
pub(super) fn repeat(round: fn()) {
    let failed: Vec<usize> = (0..6)
        .filter(|n| {
            println!("ROUND {n}");
            let failed = std::panic::catch_unwind(round).is_err();
            println!("ROUND {n} {}", if failed { "FAILED" } else { "passed" });
            failed
        })
        .collect();
    assert!(failed.is_empty(), "rounds {failed:?} failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Five batches at `p99` on a runner whose probe reads `probe`.
    fn run(p99: [u128; 5], probe: u128) -> bool {
        tail_holds(&p99.map(|p99| (p99, probe)))
    }

    #[test]
    fn main_on_a_normal_runner_passes() {
        assert!(run([31, 34, 35, 36, 35], 12));
        assert!(run([28, 37, 37, 29, 28], 15));
        assert!(run([20, 23, 21, 20, 21], 10));
    }

    #[test]
    fn a_tail_several_times_slower_on_a_quiet_runner_fails() {
        // #870's p99 of 123 to 200 on a normal runner, whose probe read 12.
        assert!(!run([123, 150, 200, 160, 180], 12));
        assert!(!run([61, 61, 61, 61, 61], 5));
    }

    #[test]
    fn main_on_a_slow_runner_passes() {
        // #870's first run, a slow VM throughout: p99 180 to 246, and its probe slowed with it.
        assert!(run([183, 226, 233, 180, 246], 90));
    }

    #[test]
    fn a_regression_on_a_slow_runner_still_fails() {
        assert!(!run([900, 1_000, 950, 1_100, 980], 90));
    }

    #[test]
    fn up_to_two_noisy_batches_are_tolerated() {
        assert!(tail_holds(&[(24_107, 12), (26, 12), (237_676, 12), (24, 12), (29, 12)]));
        assert!(!tail_holds(&[(24_107, 12), (26, 12), (237_676, 12), (24_000, 12), (29, 12)]));
    }
}
