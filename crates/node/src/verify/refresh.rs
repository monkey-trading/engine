//! The node's side of the collateral relay: fetch the vendor collateral for the node's own quote
//! at boot, refresh it every `tee.collateral_refresh`, and serve the last good copy at
//! `GET /v1/attestation/collateral`.
//!
//! The host controls the network, so every input here is untrusted and bounded:
//! - the vendor's answers: each is capped at 1 MiB and 15 s, retried three times, with no redirect
//!   or proxy, over TLS to the compiled-in public roots (`Collateral`), then each item is capped
//!   again at what a vendor certificate or list can be (`RelayedCollateral`);
//! - what is fetched: a candidate replaces the served copy only if it links, binds to this
//!   node's quote, carries a current list, and the VCEK signed the quote
//!   (`RelayedCollateral::check`); anything else is dropped. The one candidate that fails the
//!   check and is served anyway is otherwise-valid collateral whose authentic list revokes the
//!   intermediate: the relay never hides vendor data that makes a verifier refuse this node, so
//!   verifiers learn of the revocation and the Critical `collateral_revoked` pages the operator;
//! - the clock those dates are read from: a skewed clock makes the node refuse a candidate and
//!   keep the last copy, which delays a revocation but never makes a verifier accept anything,
//!   because a verifier re-checks every date on its own clock;
//! - the refresh cadence: `tee.collateral_refresh` is bounded in config, and one refresh has a
//!   deadline, so a slow vendor cannot stall the loop or make it hammer the vendor;
//! - the service URL and processor name: config, which the attestation binds by hash.
//!
//! A blocked, slowed, or lying network therefore costs only staleness: the last good copy keeps
//! being served until a verifier's own date checks refuse it, and `collateral_stale` pages once the
//! list is within a day of its `nextUpdate`. It cannot make a verifier accept a chain that is not
//! the vendor's, because a verifier pins the vendor roots itself (see `relay`). It can delay a
//! revocation: while the node cannot refresh, or reads a skewed clock, a list that predates a new
//! revocation stays valid until its `nextUpdate`. A revocation the node does fetch reaches
//! verifiers at the next fetch.

use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Bytes;
use tokio::sync::watch;
use tokio::time::{Instant, sleep, timeout};
use tracing::{error, info, warn};

use super::chain::Collateral;
use super::relay::RelayedCollateral;
use super::report::SnpReport;
use crate::config::TeeSection;
use crate::tee::Attestation;

/// The serialized collateral the route serves: `None` until the first refresh succeeds.
pub type Served = watch::Receiver<Option<Bytes>>;

/// How long one refresh may take in all, however many attempts its fetches make.
const REFRESH_DEADLINE: Duration = Duration::from_secs(5 * 60);
/// How soon a failed refresh is tried again, when sooner than the configured cadence; also the
/// shortest wait before refreshing a list that is close to its `nextUpdate`.
const SHORT_WAIT: Duration = Duration::from_secs(5 * 60);
/// How long a node may go without any collateral before failing fetches page, however long the
/// refresh cadence is.
const FIRST_FETCH_PAGE_AFTER: Duration = Duration::from_secs(30 * 60);
/// How close to the list's `nextUpdate` a failing refresh pages.
const STALE_WINDOW_SECS: i64 = 24 * 60 * 60;

/// The kernel's configfs-tsm provider name for an SEV-SNP guest report.
const SNP_PROVIDER: &str = "sev_guest";

/// One refresh's result: the document to serve, the list's `nextUpdate`, and whether its authentic
/// list revokes the intermediate.
#[derive(Debug)]
pub(super) struct Refreshed {
    pub(super) json: Bytes,
    pub(super) next_update: Option<i64>,
    pub(super) revoked: bool,
}

/// A node's collateral relay: what to fetch, and where the result goes.
pub struct Relay {
    quote: Vec<u8>,
    processor: String,
    kds_base: String,
    refresh: Duration,
    served: watch::Sender<Option<Bytes>>,
}

impl Relay {
    /// A relay for `attestation`'s quote, or `None` when the provider is not SEV-SNP (a mock or TDX
    /// quote has no vendor collateral here, and the route answers 404).
    /// Error cases:
    /// - the quote cannot be decoded
    /// - the provider is `sev_guest` and the quote is not a supported SEV-SNP report
    /// - the quote is an SEV-SNP report and `tee.processor` is unset
    pub fn new(tee: &TeeSection, attestation: &Attestation) -> Result<Option<Self>> {
        let quote = hex::decode(attestation.quote.strip_prefix("0x").unwrap_or(&attestation.quote))
            .context("decoding the quote")?;
        if attestation.provider != SNP_PROVIDER {
            return Ok(None);
        }
        SnpReport::parse(&quote).context("the sev_guest quote is not a supported SEV-SNP report")?;
        let processor = tee.processor.clone().context("tee.processor is required with an SEV-SNP quote")?;
        Ok(Some(Self {
            quote,
            processor,
            kds_base: tee.kds_base.clone(),
            refresh: tee.collateral_refresh,
            served: watch::channel(None).0,
        }))
    }

    /// The receiver the route reads.
    pub fn served(&self) -> Served {
        self.served.subscribe()
    }

    /// Refreshes the collateral forever. A failed refresh keeps the last copy and is retried; the
    /// loop ends only if the vendor client cannot be built.
    /// Error cases:
    /// - the HTTP client cannot be built
    pub async fn run(self) -> Result<()> {
        let started = Instant::now();
        let collateral = Collateral::new(&self.kds_base, &self.processor)?;
        // The held list's `nextUpdate` in Unix seconds, once a refresh has succeeded.
        let mut held: Option<Option<i64>> = None;
        loop {
            let wait = match self.refresh_once(&collateral).await {
                Ok(fetched) => {
                    info!(
                        next_update = fetched.next_update,
                        bytes = fetched.json.len(),
                        "attestation collateral refreshed"
                    );
                    info!(alert_resolved = "collateral_stale", "attestation collateral is current");
                    if fetched.revoked {
                        error!(
                            alert = "collateral_revoked",
                            "the vendor revoked this node's attestation chain; verifiers now refuse the quote"
                        );
                    } else {
                        info!(alert_resolved = "collateral_revoked", "attestation chain is not revoked");
                    }
                    held = Some(fetched.next_update);
                    self.served.send_replace(Some(fetched.json));
                    next_wait(self.refresh, fetched.next_update, unix_now())
                }
                Err(err) => {
                    warn!("attestation collateral refresh failed: {err:#}");
                    if is_stale(held, started.elapsed(), self.refresh, unix_now()) {
                        warn!(
                            alert = "collateral_stale",
                            "attestation collateral is stale or missing and refreshing fails: {err:#}"
                        );
                    }
                    self.refresh.min(SHORT_WAIT)
                }
            };
            sleep(wait).await;
        }
    }

    /// One bounded fetch and check of the collateral for this node's quote.
    /// Error cases:
    /// - the refresh exceeds its deadline, or the fetched collateral fails its checks other than an
    ///   authentic revocation of the intermediate, which is returned with `revoked` set
    pub(super) async fn refresh_once(&self, collateral: &Collateral<'_>) -> Result<Refreshed> {
        let report = SnpReport::parse(&self.quote)?;
        let fetched = timeout(REFRESH_DEADLINE, RelayedCollateral::fetch(collateral, &report))
            .await
            .context("the refresh exceeded its deadline")??;
        let revoked = match fetched.check(&report) {
            Ok(()) => false,
            Err(err) if RelayedCollateral::is_revoked(&err) => true,
            Err(err) => return Err(err.context("the fetched collateral is not valid for this node's quote")),
        };
        Ok(Refreshed { json: Bytes::from(fetched.to_json()), next_update: fetched.crl_next_update()?, revoked })
    }
}

/// Whether failing to refresh now deserves a page: the held list is within a day of its
/// `nextUpdate`, or nothing has been fetched for a refresh interval (at most 30 minutes) since
/// start. It is checked when a refresh fails, so the page lands on the first failure past that mark.
pub(super) fn is_stale(held: Option<Option<i64>>, uptime: Duration, refresh: Duration, now_secs: i64) -> bool {
    match held {
        Some(next_update) => next_update.is_some_and(|next| next - now_secs <= STALE_WINDOW_SECS),
        None => uptime >= refresh.min(FIRST_FETCH_PAGE_AFTER),
    }
}

/// How long to wait after a good refresh: the configured cadence, or half the time left to the
/// list's `nextUpdate` when that is sooner (never under five minutes, however close), so a
/// replacement list is fetched before the served one expires.
pub(super) fn next_wait(refresh: Duration, next_update: Option<i64>, now_secs: i64) -> Duration {
    let Some(next) = next_update else { return refresh };
    let half_left = Duration::from_secs(((next - now_secs) / 2).max(0) as u64);
    refresh.min(half_left.max(SHORT_WAIT.min(refresh)))
}

/// The host's clock in Unix seconds, for dates only: a skew makes the node refuse or page, never accept.
fn unix_now() -> i64 {
    (crate::clock::now_ms() / 1000) as i64
}
