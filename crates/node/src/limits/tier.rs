//! Request buckets for accounts at a volume tier's rate (`api.request_bursts`).

use std::num::NonZeroU32;
use std::sync::{Arc, OnceLock};

use engine_core::types::AccountId;
use engine_core::volume_tier::{MAX_REQUESTS_PER_SECOND, RequestLimit};
use governor::clock::Clock;
use governor::{Quota, RateLimiter};

use crate::api::ApiError;
use crate::config::RateLimit;
use crate::limits::{KeyedLimiter, check};

/// Request buckets for accounts at a volume tier's rate. A bucket's rate is fixed when it is made, so
/// there is one limiter for each rate a tier may name (`1..=MAX_REQUESTS_PER_SECOND`), made when first
/// used; an account is counted in the limiter of its tier's rate now, so one whose tier changes is
/// counted in that rate's bucket, full unless it was spent there recently, and the one it left is
/// forgotten once it has refilled.
pub(super) struct TierBuckets<C: Clock> {
    pub(super) by_rate: Box<[OnceLock<KeyedLimiter<AccountId, C>>]>,
    /// Seconds of the rate a bucket holds.
    burst_seconds: NonZeroU32,
    quota: Arc<dyn Fn(RateLimit) -> Quota + Send + Sync>,
    clock: C,
    /// Names the bucket in the log.
    name: &'static str,
}

impl<C: Clock + Clone> TierBuckets<C> {
    /// Buckets of `burst_seconds` of each rate, made by `quota` on `clock` and named `name` in the log.
    pub(super) fn new(
        name: &'static str,
        burst_seconds: NonZeroU32,
        quota: Arc<dyn Fn(RateLimit) -> Quota + Send + Sync>,
        clock: C,
    ) -> Self {
        let by_rate = (0..MAX_REQUESTS_PER_SECOND).map(|_| OnceLock::new()).collect();
        Self { by_rate, burst_seconds, quota, clock, name }
    }

    /// Takes one request from `account`'s bucket at `limit`'s rate.
    /// Error cases:
    /// - the bucket is empty: `TierLimited`, with the wait until it holds a request again
    pub(super) fn check(&self, account: AccountId, limit: RequestLimit) -> Result<(), ApiError> {
        // The engine validates every tier's rate within 1..=MAX_REQUESTS_PER_SECOND.
        let rate = NonZeroU32::new(limit.per_second).expect("a tier's request rate is nonzero");
        let slot = usize::try_from(rate.get() - 1).ok().and_then(|at| self.by_rate.get(at));
        let slot = slot.expect("the engine bounds a tier's request rate to 1..=MAX_REQUESTS_PER_SECOND");
        let limiter = slot.get_or_init(|| {
            let burst = rate.saturating_mul(self.burst_seconds);
            RateLimiter::dashmap_with_clock((self.quota)(RateLimit { per_second: rate, burst }), self.clock.clone())
        });
        check(limiter, &account, self.name).map_err(|e| match e {
            ApiError::RateLimited(wait) => ApiError::TierLimited { wait, tier: limit.tier, per_second: rate.get() },
            other => other,
        })
    }

    /// Forgets accounts whose buckets have refilled.
    pub(super) fn retain_recent(&self) {
        self.by_rate.iter().filter_map(OnceLock::get).for_each(|limiter| limiter.retain_recent());
    }
}
