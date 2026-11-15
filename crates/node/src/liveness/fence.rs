//! The operator's fence by its own clock. Each successful read of the chain gives `E`, the schedule's
//! `promotionDueAt`: the earliest block time a promotion could pass from the heads, the signer change
//! and the marks on chain at that block, assuming every later slot is marked. A read at an older block
//! can only give an earlier `E`, since heads and unmarked slots only move it later, so a stale or
//! failed read never moves the fence later. The enclave stops once its upper bound on true time
//! (`TrustedClock::upper_bound_ms`, widened by the slowest permitted clock rate) reaches `E - DELTA`,
//! with `DELTA` covering how far block timestamps may lead true time and the time to stop. A later read
//! giving a later `E` reopens it, while the vault still names this enclave's key.
//!
//! A read under which no promotion can pass holds only until a drawn turn could let one: with a set of
//! no other member, until the later of the next turn boundary after its block and `E`; with no turn
//! naming the operator, or no schedule, until `2 x DELTA` past that boundary, since no slot can be
//! witnessed before a turn names it.

use engine_core::types::AccountId;

use crate::liveness::DELTA;

/// What the last successful read fixed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Fence {
    /// `E`, in milliseconds; `None` before the first read.
    due_ms: Option<u64>,
    /// The vault named another key: this process signs nothing more.
    replaced: bool,
}

impl Fence {
    /// Takes a read whose `promotionDueAt` is `due_at` seconds and whose vault names `signer`, against
    /// this enclave's `key`. A vault naming another key fences this process for good.
    pub(crate) fn read(&mut self, due_at: u64, signer: AccountId, key: AccountId) {
        self.named(signer, key);
        self.due_ms = Some(due_at.saturating_mul(1000));
    }

    /// Takes a read whose vault names `signer`, against this enclave's `key`, whether or not the rule
    /// applies under it: a vault naming another key fences this process for good. A retired signer
    /// (zero) names no other key: the guardian's retirement leaves the node sequencing, and no
    /// promotion passes while it lasts.
    pub(crate) fn named(&mut self, signer: AccountId, key: AccountId) {
        self.replaced |= signer != key && signer != AccountId([0; 20]);
    }

    /// Takes a read under which no promotion can pass before `until` seconds, at or past the next turn
    /// boundary. It leaves a vault that named another key as it was.
    pub(crate) fn exempt(&mut self, until: u64) {
        self.due_ms = Some(until.saturating_mul(1000));
    }

    /// Whether the fence stops everything at `upper`, the enclave's upper bound on true time in
    /// milliseconds: before the first read, without a trusted time sample, once the bound reaches
    /// `E - DELTA`, and for good once the vault named another key.
    pub(crate) fn fenced(&self, upper: Option<i64>) -> bool {
        let (Some(due), Some(upper)) = (self.due_ms, upper.and_then(|upper| u64::try_from(upper).ok())) else {
            return true;
        };
        let delta = u64::try_from(DELTA.as_millis()).expect("DELTA fits in milliseconds");
        self.replaced || upper.saturating_add(delta) >= due
    }

    /// `E`, in milliseconds, if read.
    pub(crate) fn due_ms(&self) -> Option<u64> {
        self.due_ms
    }

    /// Whether the vault named another key.
    pub(crate) fn replaced(&self) -> bool {
        self.replaced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: AccountId = AccountId([1; 20]);
    /// `E`, in seconds: five slots of 60 s after the head's slot.
    const DUE_S: u64 = 1_800_000_360;

    #[test]
    fn nothing_is_signed_before_the_first_read_or_without_a_sample() {
        let mut fence = Fence::default();
        assert!(fence.fenced(Some(0)));
        fence.read(DUE_S, KEY, KEY);
        assert!(fence.fenced(None));
        assert!(!fence.fenced(Some(0)));
    }

    #[test]
    fn the_fence_closes_at_e_minus_delta_and_a_later_e_reopens_it() {
        let mut fence = Fence::default();
        fence.read(DUE_S, KEY, KEY);
        let due_ms = i64::try_from(DUE_S * 1000).unwrap();
        let delta = i64::try_from(DELTA.as_millis()).unwrap();
        assert!(!fence.fenced(Some(due_ms - delta - 1)));
        assert!(fence.fenced(Some(due_ms - delta)), "the stop is at E - DELTA by the upper bound");
        // A failed read changes nothing: time runs on towards the same E.
        assert!(fence.fenced(Some(due_ms)));
        // A head landed: the next read's E is a slot later.
        fence.read(DUE_S + 60, KEY, KEY);
        assert!(!fence.fenced(Some(due_ms)));
    }

    #[test]
    fn an_exemption_holds_until_its_boundary_and_keeps_a_replaced_key_fenced() {
        let mut fence = Fence::default();
        fence.exempt(DUE_S);
        let due_ms = i64::try_from(DUE_S * 1000).unwrap();
        let delta = i64::try_from(DELTA.as_millis()).unwrap();
        assert!(!fence.fenced(Some(due_ms - delta - 1)));
        assert!(fence.fenced(Some(due_ms - delta)), "an exemption lapses at the boundary less DELTA");
        fence.read(DUE_S, AccountId([2; 20]), KEY);
        fence.exempt(DUE_S + 86_400);
        assert!(fence.fenced(Some(0)), "an exemption does not undo a promotion away from this key");
    }

    /// Under the slowest clock the rule permits, with true time at the top of the sample's interval
    /// and any time since the sample, the upper bound never trails true time: whatever `E`, everything
    /// has stopped by true time `E - DELTA`, which leaves the fence tick's latency inside the 5 s
    /// between it and `E - 10 s`, the furthest block timestamps may lead true time.
    #[test]
    fn under_the_slowest_clock_the_stop_comes_by_e_minus_delta_in_true_time() {
        use std::sync::Arc;
        use std::time::Duration;

        use crate::clock::test_clock::TestClock;
        use crate::clock::{Clock, SystemClock};
        use crate::liveness::MAX_SLOW_PPM;
        use crate::trusted_time::{Sample, TrustedClock};

        const SAMPLED_S: u64 = 1_800_000_000;
        let delta_ms = u64::try_from(DELTA.as_millis()).unwrap();
        for radius_ms in [0_u64, 1_000, 10_000] {
            for due_after_s in [16_u64, 17, 61, 3_601, 86_401] {
                let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
                let sampled_ms = SAMPLED_S * 1000;
                let sample = Sample {
                    at: clock.instant(),
                    earliest_ms: i64::try_from(sampled_ms - 2 * radius_ms).unwrap(),
                    latest_ms: i64::try_from(sampled_ms).unwrap(),
                };
                let trusted = TrustedClock::new(
                    clock.clone(),
                    tokio::sync::watch::channel(Some(sample)).1,
                    Duration::from_secs(10),
                );
                let mut fence = Fence::default();
                fence.read(SAMPLED_S + due_after_s, KEY, KEY);
                // True time runs from the sample's top to `E - DELTA` while the monotonic clock runs
                // MAX_SLOW_PPM slow.
                let true_ns = u128::from((due_after_s * 1000).saturating_sub(delta_ms)) * 1_000_000;
                let monotonic_ns = true_ns * u128::from(1_000_000 - MAX_SLOW_PPM) / 1_000_000;
                clock.advance(Duration::from_nanos(u64::try_from(monotonic_ns).unwrap()));
                let upper = trusted.upper_bound_ms(MAX_SLOW_PPM);
                let true_ms = i64::try_from(sampled_ms + (due_after_s * 1000).saturating_sub(delta_ms)).unwrap();
                assert!(upper >= Some(true_ms), "radius {radius_ms} ms, E {due_after_s} s on: {upper:?} < {true_ms}");
                assert!(fence.fenced(upper), "radius {radius_ms} ms, E {due_after_s} s on: open at E - DELTA");
            }
        }
    }

    #[test]
    fn a_retired_signer_names_no_other_key() {
        let mut fence = Fence::default();
        fence.read(DUE_S, AccountId([0; 20]), KEY);
        assert!(!fence.replaced(), "the guardian's retirement ended the run as a replacement");
        assert!(!fence.fenced(Some(0)));
    }

    #[test]
    fn a_vault_naming_another_key_fences_for_good() {
        let mut fence = Fence::default();
        fence.read(DUE_S, AccountId([2; 20]), KEY);
        assert!(fence.fenced(Some(0)));
        fence.read(DUE_S + 600, KEY, KEY);
        assert!(fence.fenced(Some(0)), "a promotion away from this key is not undone by a later read");
        assert!(fence.replaced());
    }
}
