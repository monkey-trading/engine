//! Fetching a quote's collateral from the vendor's key-distribution service: the VCEK by the chip
//! id and reported TCB the quote carries, the intermediate and root, and the root's revocation
//! list. Nothing fetched here is trusted: `engine_verify` checks it against roots pinned in the
//! binary, so the service, a mirror, or the network can make a verification fail, never pass.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use engine_verify::{RelayedCollateral, SnpReport, pem_certificates};
use futures_util::StreamExt as _;
use reqwest::Client;
use reqwest::redirect::Policy;
use rustls::ClientConfig;
use rustls::RootCertStore;

/// The vendor's key-distribution service, which issues a chip's VCEK and serves the intermediate
/// and root certificates.
pub const KDS_BASE_URL: &str = "https://kdsintf.amd.com";

/// How long one collateral fetch may take, how often a transient failure is retried, and the most
/// bytes an answer may carry.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const FETCH_ATTEMPTS: usize = 3;
const FETCH_BACKOFF: Duration = Duration::from_millis(500);
pub(super) const MAX_COLLATERAL_BYTES: usize = 1024 * 1024;

/// Fetches a quote's certificate chain from the vendor's key-distribution service. TLS runs on
/// rustls with the compiled-in public roots, so the host's trust store plays no part; the client
/// uses no proxy, follows no redirect, and gives every request a deadline.
pub struct Collateral<'a> {
    client: Client,
    base: &'a str,
    processor: &'a str,
}

impl<'a> Collateral<'a> {
    /// A client for the service at `base` serving processors named `processor`.
    /// Error cases:
    /// - the TLS or HTTP client cannot be built
    pub fn new(base: &'a str, processor: &'a str) -> anyhow::Result<Self> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("TLS protocol versions")?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client = Client::builder()
            // Vendors refuse a request without one, and the client sends none by default.
            .user_agent(concat!("engine-verify/", env!("CARGO_PKG_VERSION")))
            .tls_backend_preconfigured(tls)
            .timeout(FETCH_TIMEOUT)
            .redirect(Policy::none())
            .no_proxy()
            .build()
            .context("building the collateral client")?;
        Ok(Self { client, base, processor })
    }

    /// Fetches `report`'s collateral, then bounds each item (see `RelayedCollateral::ensure_bounded`).
    /// Error cases:
    /// - the service does not answer, or an item exceeds its bound
    pub async fn relayed(&self, report: &SnpReport<'_>) -> anyhow::Result<RelayedCollateral> {
        let (intermediate, root) = self.issuer_chain().await.context("fetching the issuer chain")?;
        let vcek = self.vcek(report).await.context("issuing the VCEK")?;
        let crl = self.crl().await.context("fetching the certificate revocation list")?;
        let fetched = RelayedCollateral { vcek, intermediate, root, crl, fetched_at: crate::clock::now_ms() };
        fetched.ensure_bounded()?;
        Ok(fetched)
    }

    /// The VCEK for `report`'s chip and reported TCB. Issuing is keyed on exactly those, so the
    /// served certificate is bound to the chip that produced the report.
    /// Error cases:
    /// - the service does not answer, or answers without a certificate
    pub async fn vcek(&self, report: &SnpReport<'_>) -> anyhow::Result<Vec<u8>> {
        let tcb = report.reported_tcb;
        let url = format!(
            "{}/vcek/v1/{}/{}?blSPL={}&teeSPL={}&snpSPL={}&ucodeSPL={}",
            self.base.trim_end_matches('/'),
            self.processor,
            hex::encode(report.chip_id),
            tcb.boot_loader,
            tcb.tee,
            tcb.snp,
            tcb.microcode
        );
        self.fetch_cert(&url, "VCEK").await
    }

    /// The intermediate and root certificates for this processor, issuer chain order. The vendor
    /// serves these as PEM, unlike the VCEK endpoint's DER.
    /// Error cases:
    /// - the service does not answer, or answers without both certificates
    pub async fn issuer_chain(&self) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        let url = format!("{}/vcek/v1/{}/cert_chain", self.base.trim_end_matches('/'), self.processor);
        let body = self.fetch(&url).await?;
        let pem = std::str::from_utf8(&body).context("collateral PEM is not ASCII")?;
        let certs = pem_certificates(pem)?;
        let [intermediate, root] = certs.as_slice() else {
            bail!("cert chain answered with {} certificates, want 2", certs.len());
        };
        Ok((intermediate.clone(), root.clone()))
    }

    /// One certificate, accepting the vendor's VCEK endpoint DER or the PEM other endpoints serve.
    /// Error cases:
    /// - the service does not answer, or answers without a certificate
    async fn fetch_cert(&self, url: &str, what: &str) -> anyhow::Result<Vec<u8>> {
        let body = self.fetch(url).await?;
        if body.starts_with(b"-----BEGIN") {
            pem_certificates(std::str::from_utf8(&body).context("collateral PEM is not ASCII")?)?
                .into_iter()
                .next()
                .with_context(|| format!("collateral answered without a {what} certificate"))
        } else {
            Ok(body)
        }
    }

    /// The processor's certificate revocation list as DER, decoding the PEM the vendor serves
    /// it as.
    /// Error cases:
    /// - the service does not answer, or answers without a list
    pub async fn crl(&self) -> anyhow::Result<Vec<u8>> {
        let url = format!("{}/vcek/v1/{}/crl", self.base.trim_end_matches('/'), self.processor);
        let body = self.fetch(&url).await?;
        if body.starts_with(b"-----BEGIN") {
            let mut blocks = x509_parser::pem::Pem::iter_from_buffer(&body);
            let block = blocks
                .next()
                .context("collateral answered without a revocation list")?
                .map_err(|err| anyhow::anyhow!("malformed PEM: {err}"))?;
            return Ok(block.contents);
        }
        Ok(body)
    }

    /// The body served at `url`, fetched like any collateral: bounded, without redirects, and
    /// retried only on a transient failure.
    /// Error cases:
    /// - see `fetch`
    pub async fn document(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        self.fetch(url).await
    }

    /// Fetches `url`, retrying a transient failure — a transport or body-read error, 429, or 5xx
    /// — with a short backoff, and refusing a 4xx without another try. The body is read in
    /// bounded chunks, never buffered past `MAX_COLLATERAL_BYTES`.
    /// Error cases:
    /// - the service keeps failing, answers 4xx, or sends an oversized body
    async fn fetch(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let mut last = None;
        for attempt in 0..FETCH_ATTEMPTS {
            match self.attempt(url).await {
                Ok(body) => return Ok(body),
                Err(FetchError::Transient(message)) => {
                    last = Some(message);
                    if attempt + 1 < FETCH_ATTEMPTS {
                        tokio::time::sleep(FETCH_BACKOFF).await;
                    }
                }
                Err(FetchError::Refused(message)) => return Err(anyhow::anyhow!(message)),
            }
        }
        Err(CollateralUnavailable(last.expect("at least one attempt")).into())
    }

    /// One fetch attempt.
    /// Error cases:
    /// - `Transient`: a transport or body-read error, 429, or 5xx, worth another attempt
    /// - `Refused`: a 4xx or an oversized body, not worth one
    async fn attempt(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        let response =
            self.client.get(url).send().await.map_err(|err| FetchError::Transient(format!("fetching {url}: {err}")))?;
        let status = response.status();
        if !status.is_success() {
            let message = format!("{url} answered {status}");
            return Err(if status.is_server_error() || status.as_u16() == 429 {
                FetchError::Transient(message)
            } else {
                FetchError::Refused(message)
            });
        }
        if let Some(length) = response.content_length()
            && length as usize > MAX_COLLATERAL_BYTES
        {
            return Err(FetchError::Refused(format!("{url} answered with an oversized body")));
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) =
            stream.next().await.transpose().map_err(|err| FetchError::Transient(format!("reading {url}: {err}")))?
        {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_COLLATERAL_BYTES {
                return Err(FetchError::Refused(format!("{url} answered with an oversized body")));
            }
        }
        Ok(body)
    }
}

/// The service kept failing in a way worth retrying (transport, 429 or 5xx): it supplied no
/// collateral, which says nothing about the quote. A refusal (4xx) is not this error.
#[derive(Debug)]
pub(crate) struct CollateralUnavailable(pub(crate) String);

impl std::fmt::Display for CollateralUnavailable {
    /// The last failure.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CollateralUnavailable {}

/// How an attempt failed: worth retrying, or not.
enum FetchError {
    Transient(String),
    Refused(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A vendor refuses a request without a User-Agent, so every fetch sends one.
    #[tokio::test]
    async fn fetches_carry_a_user_agent() {
        let app = axum::Router::new().route(
            "/document",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                let agent = headers.get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok());
                assert_eq!(agent, Some(concat!("engine-verify/", env!("CARGO_PKG_VERSION"))));
                "collateral"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = format!("http://{addr}");
        let collateral = Collateral::new(&base, "Milan").unwrap();
        let body = collateral.document(&format!("{base}/document")).await.unwrap();
        assert_eq!(body, b"collateral");
    }
}
