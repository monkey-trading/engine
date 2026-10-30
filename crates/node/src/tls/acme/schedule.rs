//! When the certificate task renews, and what it alerts. Time is the monotonic clock from the
//! finalize request of a certificate's order, which precedes its issuance, against the
//! certificate's own validity period, so the host's wall clock plays no part; the monotonic clock
//! carries the TODO in `clock::Clock::instant`. A certificate restored from the sealed keys after a
//! restart is aged by trusted time instead. A CA that backdates `notBefore` makes the schedule
//! late by as much, which `renew_before`'s floor and the default third of the certificate's life
//! leave well before expiry.

use std::time::Duration;

use prometheus_client::metrics::gauge::Gauge;
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
    /// When the served CA certificate expires.
    expires: Option<Instant>,
    failures: u32,
    /// Mirrors `certificate_pending` for the operator's Prometheus: 1 until the first certificate.
    pending: Gauge,
}

impl Schedule {
    /// No CA certificate yet: raises `certificate_pending`, which the first issuance resolves, and
    /// sets `pending`.
    pub(super) fn start(domain: String, renew_before: Option<Duration>, pending: Gauge) -> Self {
        warn!(
            alert = "certificate_pending",
            domain = %domain,
            "serving the self-signed certificate until the CA issues one for the attested key"
        );
        pending.set(1);
        Self { domain, renew_before, expires: None, failures: 0, pending }
    }

    /// A CA certificate restored at `now`, `age` into its `lifetime`, already served: no
    /// `certificate_pending`, and the end of one an earlier process left open. Returns the schedule
    /// and how long to wait before renewing it, as `issued` does.
    pub(super) fn restored(
        domain: String,
        renew_before: Option<Duration>,
        pending: Gauge,
        lifetime: Duration,
        age: Duration,
        now: Instant,
    ) -> (Self, Duration) {
        pending.set(0);
        info!(alert_resolved = "certificate_pending", domain = %domain, "serving the sealed CA certificate");
        let mut schedule = Self { domain, renew_before, expires: None, failures: 0, pending };
        let wait = schedule.serve(lifetime, now + lifetime.saturating_sub(age), now);
        (schedule, wait)
    }

    /// Records a certificate valid for `lifetime`, counted from `finalized`, installed at `now`,
    /// ends a pending or expiring episode, and returns how long to wait before renewing it: until
    /// `renew_before` of its life remains, or one third when that is unset or not shorter than its
    /// life; no wait once that much has passed.
    pub(super) fn issued(&mut self, lifetime: Duration, finalized: Instant, now: Instant) -> Duration {
        if self.expires.is_none() {
            self.pending.set(0);
            info!(alert_resolved = "certificate_pending", domain = %self.domain, "the CA issued the first certificate");
        }
        // Also ends an episode an earlier process left open in the alert outbox.
        info!(alert_resolved = "certificate_expiring", domain = %self.domain, "the CA issued a certificate");
        self.failures = 0;
        self.serve(lifetime, finalized + lifetime, now)
    }

    /// Records a served certificate valid for `lifetime` that `expires`, and returns how long from
    /// `now` until `renew_before` of its life remains, or one third when that is unset or not shorter
    /// than its life; no wait once that much has passed.
    fn serve(&mut self, lifetime: Duration, expires: Instant, now: Instant) -> Duration {
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
        self.expires = Some(expires);
        expires.checked_sub(renew_before).map_or(Duration::ZERO, |renew_at| renew_at.saturating_duration_since(now))
    }

    /// Records a failed attempt at `now` and returns how long to wait before the next. Before the
    /// first certificate it raises `certificate_pending`; after it, `certificate_expiring` once less
    /// than `EXPIRING_WITHIN` of the served certificate's life remains, which keeps serving.
    pub(super) fn failed(&mut self, error: &anyhow::Error, now: Instant) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let retry = RETRY_FIRST.saturating_mul(1 << (self.failures - 1).min(6)).min(RETRY_MAX);
        let domain = &self.domain;
        match self.expires {
            None => warn!(
                alert = "certificate_pending",
                domain = %domain,
                "no CA certificate yet, serving the self-signed one; retrying in {retry:?}: {error:#}"
            ),
            Some(expires) => {
                let remaining = expires.saturating_duration_since(now);
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
