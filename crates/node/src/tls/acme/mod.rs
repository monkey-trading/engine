//! The API's public certificate, over ACME (RFC 8555) from inside the enclave. The certificate
//! task registers an account under a key it generates in memory, orders a certificate for
//! `[api.tls.acme].domain`, answers the CA's TLS-ALPN-01 challenge on the API's own listener, and
//! finalizes the order with a request signed by the API's key, so the issued certificate is for the
//! key the attestation names. It installs the certificate without a restart, and renews it the
//! same way. Neither key is written anywhere or logged; a restart makes both again, and so orders
//! a new certificate.
//!
//! The sequencer never waits for it. Until the first certificate the API serves its self-signed
//! one, under `certificate_pending`; a failed renewal keeps serving the current one and retries
//! with backoff, raising `certificate_expiring` once fewer than 14 days remain. A failed attempt
//! resumes its order unless the CA ended it, so a lost exchange never costs another issuance. A
//! certificate's life counts from the order's finalize request, which precedes its issuance, so a
//! late download is never taken for a fresh certificate.
//!
//! The host carries the connections and resolves the CA's name, so it can delay or drop them;
//! `http` authenticates the CA against compiled-in roots (see its TODO on the host clock), and
//! every request and attempt has a deadline. Nothing here stops the host from having a CA certify a key of its own for the domain:
//! it forwards port 443, so it can answer the CA's TLS-ALPN-01 validation itself, as can whoever
//! controls the domain's DNS. A browser that trusts the certificate alone cannot tell; the CAA
//! record narrows issuance to one CA and method, and Certificate Transparency monitoring is to
//! detect such a certificate, outside the node.
//!
//! TODO: once the key survives restarts (#28), keep the account and the certificate with it, so a
//! restart orders nothing and a CAA `accounturi` can bind issuance to the enclave's account.

use std::time::Duration;

use anyhow::{Context, anyhow, bail, ensure};
use instant_acme::{
    Account, AuthorizationStatus, ChallengeStatus, ChallengeType, HttpClient, Identifier, NewAccount, NewOrder, Order,
    OrderStatus, Problem, RetryPolicy,
};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use tokio::time::{Instant, sleep, timeout};
use tracing::{Instrument, info, info_span};
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::FromDer;

use crate::config::AcmeSection;
use crate::metrics::FloatGauge;
use crate::tls::TlsKey;
use crate::tls::certificates::Answer;

pub(crate) mod http;
use crate::tls::acme::http::AcmeHttp;
mod schedule;
use crate::tls::acme::schedule::Schedule;

/// How long the CA gets to validate the challenge, and then to issue.
const POLL_TIMEOUT: Duration = Duration::from_secs(120);
/// One attempt, from registering to installing.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(600);
/// Problems after which the same request can succeed, so they end neither the order nor the account.
const PASSING_PROBLEMS: [&str; 3] = [
    "urn:ietf:params:acme:error:serverInternal",
    "urn:ietf:params:acme:error:rateLimited",
    "urn:ietf:params:acme:error:badNonce",
];

/// Obtains and renews the API's CA certificate, over `H`.
pub struct CertificateTask<H = AcmeHttp> {
    domain: String,
    directory_url: String,
    contact: String,
    renew_before: Option<Duration>,
    http: H,
    key: TlsKey,
    /// `notAfter` of the served CA certificate, in Unix seconds; 0 before the first.
    not_after: FloatGauge,
}

/// What attempts carry over: the account, once registered, and the order not yet issued.
#[derive(Default)]
struct Progress {
    account: Option<Account>,
    /// The URL of an order that may still issue, until it has issued or the CA ended it.
    order: Option<String>,
    /// When this process last sent the order's finalize request: no later than the CA issued.
    finalized: Option<Instant>,
}

/// A certificate the CA issued and the API now serves.
#[derive(Debug)]
struct Issued {
    /// `notAfter` less `notBefore`.
    lifetime: Duration,
    not_after_seconds: i64,
    /// When its order's finalize request was sent, from which its life is counted.
    finalized: Instant,
}

impl CertificateTask {
    /// The task for `cfg`, serving through `key`.
    /// Error cases:
    /// - the ACME HTTP client cannot be built (see `AcmeHttp::new`)
    pub fn new(cfg: &AcmeSection, key: TlsKey, not_after: FloatGauge) -> anyhow::Result<Self> {
        Ok(Self {
            domain: cfg.domain.clone(),
            directory_url: cfg.directory_url.clone(),
            contact: cfg.contact.clone(),
            renew_before: cfg.renew_before,
            http: AcmeHttp::new(cfg.test_ca.as_ref())?,
            key,
            not_after,
        })
    }
}

impl<H: HttpClient + Clone> CertificateTask<H> {
    /// Obtains the certificate, then renews it, until the node stops. Each attempt runs in its own
    /// span; a failed one is retried as `Schedule` says.
    /// Error cases:
    /// - none; it runs until the node stops
    pub async fn run(self) -> anyhow::Result<()> {
        let mut schedule = Schedule::start(self.domain.clone(), self.renew_before);
        let mut progress = Progress::default();
        let mut attempt = 0_u64;
        loop {
            attempt += 1;
            let span = info_span!("certificate", id = attempt);
            let wait = async {
                let issued = timeout(ATTEMPT_TIMEOUT, self.issue(&mut progress))
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("the attempt took over {ATTEMPT_TIMEOUT:?}")));
                match issued {
                    Ok(issued) => {
                        self.not_after.set(issued.not_after_seconds as f64);
                        let wait = schedule.issued(issued.lifetime, issued.finalized, Instant::now());
                        info!(not_after = issued.not_after_seconds, "serving the CA certificate; renewing in {wait:?}");
                        wait
                    }
                    Err(e) => schedule.failed(&e, Instant::now()),
                }
            }
            .instrument(span)
            .await;
            sleep(wait).await;
        }
    }

    /// One attempt (see `attempt`). A problem the CA answers that ends the order drops it, so the
    /// next attempt places a new one, and one that names the account as unknown drops the account
    /// too; a lost exchange or a passing problem keeps both.
    /// Error cases:
    /// - see `attempt`
    async fn issue(&self, progress: &mut Progress) -> anyhow::Result<Issued> {
        let attempted = self.attempt(progress).await;
        if let Err(problem) = attempted.as_ref().map_err(problem) {
            match problem {
                Some(problem)
                    if problem.r#type.as_deref() == Some("urn:ietf:params:acme:error:accountDoesNotExist") =>
                {
                    *progress = Progress::default();
                }
                Some(problem) if ends_order(problem) => (progress.order, progress.finalized) = (None, None),
                _ => {}
            }
        }
        attempted
    }

    /// Registers the account if this process has none, takes up the unfinished order or places one,
    /// answers its challenge, finalizes it, and installs what the CA issues.
    /// Error cases:
    /// - any request to the CA fails, or the CA refuses the account, the order, or the challenge
    /// - the CA does not validate or issue within `POLL_TIMEOUT`
    /// - the certificate is not for the domain or the key (see `check` and `TlsKey::install`), or
    ///   its life ran out before it arrived
    async fn attempt(&self, progress: &mut Progress) -> anyhow::Result<Issued> {
        let Progress { account, order: unfinished, finalized } = progress;
        let account = match account {
            Some(account) => account,
            None => account.insert(self.register().await?),
        };
        let mut order = self.order(account, unfinished, finalized).await?;
        let pem = self.complete(&mut order, finalized).await?;
        // What the order issued is taken: a certificate refused below needs a new order.
        *unfinished = None;
        let finalized = finalized.take().context("the CA issued an order this process did not finalize")?;
        let chain = CertificateDer::pem_slice_iter(pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .context("parsing the certificate chain")?;
        let (lifetime, not_after_seconds) = check(&chain, &self.domain)?;
        ensure!(finalized.elapsed() < lifetime, "the certificate's life ran out before it arrived");
        self.key.install(chain)?;
        Ok(Issued { lifetime, not_after_seconds, finalized })
    }

    /// The unfinished order, or a new one when there is none or the CA ended it.
    /// Error cases:
    /// - the unfinished order cannot be read for a lost exchange or a passing problem, or the CA
    ///   refuses a new one
    async fn order(
        &self,
        account: &Account,
        unfinished: &mut Option<String>,
        finalized: &mut Option<Instant>,
    ) -> anyhow::Result<Order> {
        if let Some(url) = unfinished.clone() {
            match account.order(url).await {
                Ok(mut order) => {
                    if order.state().status != OrderStatus::Invalid {
                        info!(order = %order.url(), "resuming the unfinished order");
                        return Ok(order);
                    }
                    info!(order = %order.url(), "the unfinished order is invalid");
                }
                Err(instant_acme::Error::Api(problem)) if ends_order(&problem) => {
                    info!("the CA no longer serves the unfinished order: {problem}")
                }
                Err(e) => return Err(e).context("reading the unfinished order"),
            }
        }
        let identifiers = [Identifier::Dns(self.domain.clone())];
        let order = account.new_order(&NewOrder::new(&identifiers)).await.context("ordering the certificate")?;
        info!(order = %order.url(), "certificate ordered");
        (*unfinished, *finalized) = (Some(order.url().to_owned()), None);
        Ok(order)
    }

    /// Takes `order` from wherever it stands to its certificate chain, PEM: validates the domain
    /// while it is pending, finalizes it with the key's request once ready, noting when in
    /// `finalized`, and downloads what it issues.
    /// Error cases:
    /// - a request to the CA fails, or the CA refuses the challenge or the order
    /// - the CA does not validate or issue within `POLL_TIMEOUT`
    async fn complete(&self, order: &mut Order, finalized: &mut Option<Instant>) -> anyhow::Result<String> {
        let retries = RetryPolicy::new().timeout(POLL_TIMEOUT);
        if order.state().status == OrderStatus::Pending {
            let answer = self.answer(order).await?;
            let status = order.poll_ready(&retries).await.context("waiting for the CA to validate the domain")?;
            drop(answer);
            if status != OrderStatus::Ready {
                bail!("the order is {status:?}{}", refusal(order).await);
            }
        }
        if order.state().status == OrderStatus::Ready {
            let request = self.key.certificate_request(&self.domain)?;
            *finalized = Some(Instant::now());
            order.finalize_csr(&request).await.context("finalizing the order")?;
        }
        order.poll_certificate(&retries).await.context("waiting for the CA to issue")
    }

    /// Answers the pending order's TLS-ALPN-01 challenge, until the returned guard drops, and tells
    /// the CA it is ready unless it already knows; nothing when the CA reuses a validation.
    /// Error cases:
    /// - the authorization cannot be read, offers no tls-alpn-01 challenge, or is neither pending
    ///   nor valid, or the CA refuses the readiness
    async fn answer(&self, order: &mut Order) -> anyhow::Result<Option<Answer<'_>>> {
        let mut authorizations = order.authorizations();
        let mut authorization = authorizations
            .next()
            .await
            .context("the order has no authorization")?
            .context("reading the authorization")?;
        match authorization.status {
            AuthorizationStatus::Valid => {
                info!("the CA reused its validation of the domain for this account");
                Ok(None)
            }
            AuthorizationStatus::Pending => {
                let mut challenge = authorization
                    .challenge(ChallengeType::TlsAlpn01)
                    .context("the CA offers no tls-alpn-01 challenge")?;
                let digest = challenge.key_authorization().digest().as_ref().try_into().context("key digest")?;
                let answer = self.key.answer(&self.domain, digest)?;
                if challenge.status == ChallengeStatus::Pending {
                    challenge.set_ready().await.context("telling the CA the challenge is ready")?;
                }
                info!("answering the tls-alpn-01 challenge");
                Ok(Some(answer))
            }
            status => bail!("the authorization is {status:?}"),
        }
    }

    /// Registers an account under a fresh key, agreeing to the CA's terms of service.
    /// Error cases:
    /// - the directory cannot be read, or the CA refuses the account
    async fn register(&self) -> anyhow::Result<Account> {
        let contact = [self.contact.as_str()];
        let new = NewAccount { contact: &contact, terms_of_service_agreed: true, only_return_existing: false };
        // The credentials returned beside the account carry its private key: dropped unread.
        let (account, _) = Account::builder_with_http(Box::new(self.http.clone()))
            .create(&new, self.directory_url.clone(), None)
            .await
            .context("registering the ACME account")?;
        info!(account = %account.id(), "ACME account registered");
        Ok(account)
    }
}

/// Why the CA refused the domain's validation, as its challenge records it, for an error message;
/// empty when it records nothing or cannot be read, since the attempt has failed either way.
async fn refusal(order: &mut Order) -> String {
    let mut authorizations = order.authorizations();
    let Some(Ok(mut authorization)) = authorizations.next().await else { return String::new() };
    match authorization.refresh().await {
        Ok(state) => state.challenges.iter().filter_map(|c| c.error.as_ref()).map(|p| format!(": {p}")).collect(),
        Err(_) => String::new(),
    }
}

/// The problem document the CA answered `e` with, if it answered rather than the exchange failing.
fn problem(e: &anyhow::Error) -> Option<&Problem> {
    e.chain().find_map(|cause| match cause.downcast_ref::<instant_acme::Error>() {
        Some(instant_acme::Error::Api(problem)) => Some(problem),
        _ => None,
    })
}

/// Whether `problem` ends the order: any but a passing one, a 5xx, or a 429.
fn ends_order(problem: &Problem) -> bool {
    let passing_type = problem.r#type.as_deref().is_some_and(|kind| PASSING_PROBLEMS.contains(&kind));
    let passing_status = problem.status.is_some_and(|status| status >= 500 || status == 429);
    !passing_type && !passing_status
}

/// The life and `notAfter`, in Unix seconds, of `chain`'s first certificate, once it names `domain`
/// and is valid for some time.
/// Error cases:
/// - the chain is empty, or its first certificate does not parse
/// - it does not name `domain`, or its `notAfter` is not after its `notBefore`
fn check(chain: &[CertificateDer<'_>], domain: &str) -> anyhow::Result<(Duration, i64)> {
    let leaf = chain.first().context("the CA returned no certificate")?;
    let (_, leaf) = X509Certificate::from_der(leaf).map_err(|e| anyhow!("parsing the certificate: {e}"))?;
    let names = leaf.subject_alternative_name().map_err(|e| anyhow!("reading the certificate's names: {e}"))?;
    let named = names.is_some_and(|names| {
        names
            .value
            .general_names
            .iter()
            .any(|name| matches!(name, GeneralName::DNSName(n) if n.eq_ignore_ascii_case(domain)))
    });
    ensure!(named, "the certificate does not name {domain}");
    let validity = leaf.validity();
    let (not_before, not_after) = (validity.not_before.timestamp(), validity.not_after.timestamp());
    let lifetime = u64::try_from(not_after.saturating_sub(not_before)).ok().filter(|seconds| *seconds > 0);
    let lifetime = lifetime.context("the certificate's notAfter is not after its notBefore")?;
    Ok((Duration::from_secs(lifetime), not_after))
}

#[cfg(test)]
mod tests;
