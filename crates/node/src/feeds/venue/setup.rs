//! What every venue connection shares: its timing from the config, the TLS client config, and the
//! reconnect loop with its backoff between attempts.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use rustls::crypto::ring;
use rustls::{ClientConfig, RootCertStore};
use tokio::time::{Instant, sleep};
use tracing::info;

use crate::config::VenuesSection;
use crate::feeds::venue::{Timing, VenueFeed};

impl Timing {
    /// The timing a venue section configures: `min_quote_spacing` is the per-symbol sequencing
    /// pace; the mock feed's tick (`feeds.interval`) never governs venue feeds.
    pub fn for_venues(venues: &VenuesSection) -> Self {
        Timing::new(venues.min_quote_spacing, venues)
    }

    fn new(min_quote_spacing: Duration, venues: &VenuesSection) -> Self {
        Timing {
            interval: min_quote_spacing,
            timeout: venues.timeout,
            reconnect_min: venues.reconnect_min,
            reconnect_max: venues.reconnect_max,
            stale_after: venues.stale_after,
            ping_interval: venues.ping_interval,
            max_delay: venues.max_delay,
        }
    }
}

impl VenueFeed {
    /// Connects and relays until the connection fails, then retries: at once after a connection that
    /// sequenced fresh quotes of its own, though never within `timing.reconnect_min` of its attempt,
    /// else after a backoff that doubles while attempts sequence none (see `next_attempt`). A drop while
    /// the feed holds its quotes back (see `live`) is no outage: it is logged, raises no alert, and the
    /// switch's being set cuts the wait short.
    /// Error cases:
    /// - the sequencer has gone away
    pub(super) async fn run(mut self) -> anyhow::Result<()> {
        let mut backoff = self.timing.reconnect_min;
        self.publish_status();
        loop {
            (self.quoted, self.sent_own) = (false, false);
            let attempted = Instant::now();
            let Err(error) = self.relay().await;
            self.report.up = false;
            self.settle().await;
            if self.tx.is_closed() {
                // The supervisor logs what the task returns, so it is redacted as a log line is.
                return Err(anyhow!(self.redact(&format!("{error:#}"))).context("sequencer gone"));
            }
            // Healthy only on its own quotes: the engine's fresh quote may be a sibling's.
            let healthy = self.quoted && self.sent_own;
            let (wait, next) = next_attempt(backoff, healthy, attempted.elapsed(), &self.timing);
            if self.live() {
                self.dropped(&error, wait);
                self.publish_status();
                sleep(wait).await;
            } else {
                let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
                let error = self.redact(&format!("{error:#}"));
                info!(venue, feed, retry_in = ?wait, "a venue connection held back before its node runs dropped: {error}");
                self.publish_status();
                self.wait_unless_live(wait).await;
            }
            backoff = next;
        }
    }
}

/// The TLS client config every venue connection shares: ring as the crypto provider, and
/// Mozilla's roots compiled into the binary, so the host's certificate store is never trusted.
///
/// TODO: certificate validity is judged by the untrusted host clock and revocation is not
/// checked, so a host holding an expired or revoked venue key could set the clock back and pose
/// as the venue; a trusted time bound closes the first.
/// Error cases:
/// - the provider supports none of the safe default protocol versions
pub fn tls_config() -> anyhow::Result<Arc<ClientConfig>> {
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// The wait before the next connection attempt, and the backoff after that, for a connection
/// attempted `lived` ago. A `healthy` connection, one that sequenced fresh quotes of its own, ran
/// until it dropped, so it reconnects as soon as `reconnect_min` has passed since it was attempted,
/// at once after any but a brief one, and the backoff starts again at `reconnect_min`. One that
/// sequenced none failed: it waits `backoff`, and the next failure doubles it up to
/// `reconnect_max`. So the backoff applies from the second failure in a row, and attempts never
/// start closer than `reconnect_min` apart, however a venue refuses or drops them.
pub(super) fn next_attempt(backoff: Duration, healthy: bool, lived: Duration, timing: &Timing) -> (Duration, Duration) {
    if healthy {
        return (timing.reconnect_min.saturating_sub(lived), timing.reconnect_min);
    }
    (backoff, next_backoff(backoff, timing.reconnect_max))
}

/// The delay after `current`: double it, capped at `max`.
pub(super) fn next_backoff(current: Duration, max: Duration) -> Duration {
    current.saturating_mul(2).min(max)
}
