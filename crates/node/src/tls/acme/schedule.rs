//! When the certificate task renews, and what it alerts. Time is the monotonic clock from the moment
//! a certificate arrived, against the certificate's own validity period, so the host's wall clock
//! plays no part; the monotonic clock carries the TODO in `clock::Clock::instant`. A CA that
//! backdates `notBefore` makes the schedule late by as much.

use std::time::Duration;

use tokio::time::Instant;
use tracing::{info, warn};

/// The wait after a first failed attempt, doubled after each further failure up to `RETRY_MAX`.
const RETRY_FIRST: Duration = Duration::from_secs(60);
const RETRY_MAX: Duration = Duration::from_secs(3600);
/// A renewal that fails with less of the certificate's life left than this raises
/// `certificate_expiring`.
pub(super) const EXPIRING_WITHIN: Duration = Duration::from_secs(14 * 86_400);

/// The served CA certificate and the attempts since the last success.
pub(super) struct Schedule {
    domain: String,
    renew_before: Option<Duration>,
    /// When the served CA certificate arrived, and how long it is valid for.
    served: Option<(Instant, Duration)>,
    failures: u32,
}

impl Schedule {
    /// No CA certificate yet: raises `certificate_pending`, which the first issuance resolves.
    pub(super) fn start(domain: String, renew_before: Option<Duration>) -> Self {
        warn!(
            alert = "certificate_pending",
            domain = %domain,
            "serving the self-signed certificate until the CA issues one for the attested key"
        );
        Self { domain, renew_before, served: None, failures: 0 }
    }

    /// Records a certificate valid for `lifetime` that arrived at `now`, ends a pending or
    /// expiring episode, and returns how long to wait before renewing it: until `renew_before`
    /// of its life remains, or one third when that is unset or not shorter than its life.
    pub(super) fn issued(&mut self, lifetime: Duration, now: Instant) -> Duration {
        let renew_before = match self.renew_before {
            Some(before) if before < lifetime => before,
            Some(before) => {
                warn!(
                    domain = %self.domain,
                    "api.tls.acme.renew_before ({before:?}) is not shorter than the certificate's life ({lifetime:?}); \
                     renewing at one third of it"
                );
                lifetime / 3
            }
            None => lifetime / 3,
        };
        if self.served.is_none() {
            info!(alert_resolved = "certificate_pending", domain = %self.domain, "the CA issued the first certificate");
        }
        // Also ends an episode an earlier process left open in the alert outbox.
        info!(alert_resolved = "certificate_expiring", domain = %self.domain, "the CA issued a certificate");
        self.served = Some((now, lifetime));
        self.failures = 0;
        lifetime - renew_before
    }

    /// Records a failed attempt at `now` and returns how long to wait before the next. Before the
    /// first certificate it raises `certificate_pending`; after it, `certificate_expiring` once less
    /// than `EXPIRING_WITHIN` of the served certificate's life remains, which keeps serving.
    pub(super) fn failed(&mut self, error: &anyhow::Error, now: Instant) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let retry = RETRY_FIRST.saturating_mul(1 << (self.failures - 1).min(6)).min(RETRY_MAX);
        let domain = &self.domain;
        match self.served {
            None => warn!(
                alert = "certificate_pending",
                domain = %domain,
                "no CA certificate yet, serving the self-signed one; retrying in {retry:?}: {error:#}"
            ),
            Some((arrived, lifetime)) => {
                let remaining = lifetime.saturating_sub(now.saturating_duration_since(arrived));
                if remaining < EXPIRING_WITHIN {
                    warn!(
                        alert = "certificate_expiring",
                        domain = %domain,
                        "the served certificate expires in {remaining:?} and renewing it failed; retrying in \
                         {retry:?}: {error:#}"
                    );
                } else {
                    warn!(domain = %domain, "certificate renewal failed; retrying in {retry:?}: {error:#}");
                }
            }
        }
        retry
    }
}
