//! The operator's fence by its own clock. Each successful read of the chain gives `E`, the schedule's
//! `promotionDueAt`: the earliest block time a promotion could pass from the heads, the signer change
//! and the marks on chain at that block, assuming every later slot is marked. A read at an older block
//! can only give an earlier `E`, since heads and unmarked slots only move it later, so a stale or
//! failed read never moves the fence later. The enclave stops once its upper bound on true time
//! (`TrustedClock::upper_bound_ms`, widened by the slowest permitted clock rate) reaches `E - DELTA`,
//! with `DELTA` covering how far block timestamps may lead true time and the time to stop. A later read
//! giving a later `E` reopens it, while the vault still names this enclave's key.

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
        self.replaced |= signer != key;
        self.due_ms = Some(due_at.saturating_mul(1000));
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
    fn a_vault_naming_another_key_fences_for_good() {
        let mut fence = Fence::default();
        fence.read(DUE_S, AccountId([2; 20]), KEY);
        assert!(fence.fenced(Some(0)));
        fence.read(DUE_S + 600, KEY, KEY);
        assert!(fence.fenced(Some(0)), "a promotion away from this key is not undone by a later read");
        assert!(fence.replaced());
    }
}
