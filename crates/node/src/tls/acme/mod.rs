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
//! with backoff, raising `certificate_expiring` once fewer than 14 days remain.
//!
//! The host carries the connections and resolves the CA's name, so it can delay or drop them;
//! `http` authenticates the CA against compiled-in roots, and every request and attempt has a
//! deadline. Whoever controls the domain's DNS can also have a certificate issued for another key:
//! the CAA record and Certificate Transparency monitoring bound that, outside the node.
//!
//! TODO: once the key survives restarts (#28), keep the account and the certificate with it, so a
//! restart orders nothing.

mod http;
mod schedule;

use std::time::Duration;

use anyhow::{Context, anyhow, bail, ensure};
use instant_acme::{
    Account, AuthorizationStatus, ChallengeType, Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy,
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
use crate::tls::acme::http::AcmeHttp;
use crate::tls::acme::schedule::Schedule;

/// How long the CA gets to validate the challenge, and then to issue.
const POLL_TIMEOUT: Duration = Duration::from_secs(120);
/// One attempt, from registering to installing.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(600);

/// Obtains and renews the API's CA certificate.
pub struct CertificateTask {
    domain: String,
    directory_url: String,
    contact: String,
    renew_before: Option<Duration>,
    http: AcmeHttp,
    key: TlsKey,
    /// `notAfter` of the served CA certificate, in Unix seconds; 0 before the first.
    not_after: FloatGauge,
}

/// A certificate the CA issued and the API now serves.
#[derive(Debug)]
struct Issued {
    /// `notAfter` less `notBefore`.
    lifetime: Duration,
    not_after_seconds: i64,
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

    /// Obtains the certificate, then renews it, until the node stops. Each attempt runs in its own
    /// span; a failed one is retried as `Schedule` says.
    /// Error cases:
    /// - none; it runs until the node stops
    pub async fn run(self) -> anyhow::Result<()> {
        let mut schedule = Schedule::start(self.domain.clone(), self.renew_before);
        let mut account = None;
        let mut attempt = 0_u64;
        loop {
            attempt += 1;
            let span = info_span!("certificate", id = attempt);
            let wait = async {
                let issued = timeout(ATTEMPT_TIMEOUT, self.issue(&mut account))
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("the attempt took over {ATTEMPT_TIMEOUT:?}")));
                match issued {
                    Ok(issued) => {
                        self.not_after.set(issued.not_after_seconds as f64);
                        let wait = schedule.issued(issued.lifetime, Instant::now());
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

    /// One attempt: registers the account if this process has none, orders the certificate,
    /// answers its challenge, and installs what the CA issues.
    /// Error cases:
    /// - any request to the CA fails, or the CA refuses the account, the order, or the challenge
    /// - the CA does not validate or issue within `POLL_TIMEOUT`
    /// - the certificate is not for the domain or the key (see `check` and `TlsKey::install`)
    async fn issue(&self, account: &mut Option<Account>) -> anyhow::Result<Issued> {
        let account = match account {
            Some(account) => account,
            None => account.insert(self.register().await?),
        };
        let identifiers = [Identifier::Dns(self.domain.clone())];
        let mut order = account.new_order(&NewOrder::new(&identifiers)).await.context("ordering the certificate")?;
        info!(order = %order.url(), "certificate ordered");
        let answer = {
            let mut authorizations = order.authorizations();
            let mut authorization = authorizations
                .next()
                .await
                .context("the order has no authorization")?
                .context("reading the authorization")?;
            match authorization.status {
                AuthorizationStatus::Valid => {
                    info!("the CA reused its validation of the domain for this account");
                    None
                }
                AuthorizationStatus::Pending => {
                    let mut challenge = authorization
                        .challenge(ChallengeType::TlsAlpn01)
                        .context("the CA offers no tls-alpn-01 challenge")?;
                    let digest = challenge.key_authorization().digest().as_ref().try_into().context("key digest")?;
                    let answer = self.key.answer(&self.domain, digest)?;
                    challenge.set_ready().await.context("telling the CA the challenge is ready")?;
                    info!("answering the tls-alpn-01 challenge");
                    Some(answer)
                }
                status => bail!("the authorization is {status:?}"),
            }
        };
        let retries = RetryPolicy::new().timeout(POLL_TIMEOUT);
        let status = order.poll_ready(&retries).await.context("waiting for the CA to validate the domain")?;
        drop(answer);
        ensure!(status == OrderStatus::Ready, "the order is {status:?}");
        let request = self.key.certificate_request(&self.domain)?;
        order.finalize_csr(&request).await.context("finalizing the order")?;
        let pem = order.poll_certificate(&retries).await.context("waiting for the CA to issue")?;
        let chain = CertificateDer::pem_slice_iter(pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .context("parsing the certificate chain")?;
        let issued = check(&chain, &self.domain)?;
        self.key.install(chain)?;
        Ok(issued)
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

/// The validity of `chain`'s first certificate, once it names `domain` and is valid for some time.
/// Error cases:
/// - the chain is empty, or its first certificate does not parse
/// - it does not name `domain`, or its `notAfter` is not after its `notBefore`
fn check(chain: &[CertificateDer<'_>], domain: &str) -> anyhow::Result<Issued> {
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
    Ok(Issued { lifetime: Duration::from_secs(lifetime), not_after_seconds: not_after })
}

#[cfg(test)]
mod tests;
