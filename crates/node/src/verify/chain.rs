//! The quote's certificate chain: the VCEK that signed the report, the vendor intermediate that
//! signs VCEKs, and the vendor root that signs the intermediate. The roots are pinned in the
//! binary, so nothing fetched to verify a quote enters the trust set. A quote's chain is fetched
//! from the vendor's key-distribution service by the chip id and reported TCB the quote carries,
//! which binds the served VCEK to that chip and TCB.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use reqwest::Client;
use reqwest::redirect::Policy;
use rustls::ClientConfig;
use rustls::RootCertStore;
use x509_parser::certificate::X509Certificate;
use x509_parser::prelude::FromDer;

use super::report::SnpReport;

/// The vendor's key-distribution service, which issues a chip's VCEK and serves the intermediate
/// and root certificates.
pub const KDS_BASE_URL: &str = "https://kdsintf.amd.com";

/// How long one collateral fetch may take, and how often a failed fetch is retried.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const FETCH_ATTEMPTS: usize = 3;

/// Root certificates the chain is pinned to, by DER bytes. `roots` starts from the vendor roots
/// compiled into the binary; a caller may add one more, never remove one.
#[derive(Default)]
pub struct Roots {
    der: Vec<Vec<u8>>,
}

impl Roots {
    /// The vendor roots compiled into the binary.
    pub fn vendor() -> Self {
        let mut roots = Self::default();
        for pem in VENDOR_ROOT_PEMS {
            roots.der.push(parse_pem_cert(pem).expect("compiled-in vendor root parses"));
        }
        roots
    }

    /// Adds a root from ASCII PEM, for verifying against a test or private chain.
    /// Error cases:
    /// - the PEM holds no certificate
    pub fn add_pem(&mut self, pem: &str) -> anyhow::Result<()> {
        self.der.push(parse_pem_cert(pem)?);
        Ok(())
    }

    fn contains(&self, der: &[u8]) -> bool {
        self.der.iter().any(|pinned| pinned == der)
    }
}

/// The chain a quote verifies against: VCEK first, then the intermediate, then the root.
pub struct QuoteChain {
    ders: Vec<Vec<u8>>,
}

impl QuoteChain {
    /// Parses and checks `vcek`, `intermediate`, and `root` DER certificates: the root must be a
    /// compiled-in pin, each issuer name must match its parent's subject, and each signature must
    /// verify against its parent's key.
    /// Error cases:
    /// - a certificate does not parse, the root is not pinned, an issuer name does not match its
    ///   parent's subject, or a signature does not verify
    pub fn verify(vcek: Vec<u8>, intermediate: Vec<u8>, root: Vec<u8>, roots: &Roots) -> anyhow::Result<Self> {
        ensure!(roots.contains(&root), "chain root is not a pinned vendor root");
        let chain = Self { ders: vec![vcek, intermediate, root] };
        for (child, parent) in [(0, 1), (1, 2)] {
            let child = parse_cert(&chain.ders[child])?;
            let parent = parse_cert(&chain.ders[parent])?;
            ensure!(
                child.tbs_certificate.issuer() == parent.tbs_certificate.subject(),
                "certificate issuer does not match its parent's subject"
            );
            child
                .verify_signature(Some(parent.public_key()))
                .map_err(|_| anyhow::anyhow!("certificate signature does not verify against its parent"))?;
        }
        Ok(chain)
    }

    /// The VCEK's public key as its SEC 1 uncompressed point, which the verifier's signature
    /// algorithm parses.
    pub fn vcek_public_key(&self) -> anyhow::Result<Vec<u8>> {
        Ok(parse_cert(&self.ders[0])?.public_key().subject_public_key.data.to_vec())
    }
}

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
            .tls_backend_preconfigured(tls)
            .timeout(FETCH_TIMEOUT)
            .redirect(Policy::none())
            .no_proxy()
            .build()
            .context("building the collateral client")?;
        Ok(Self { client, base, processor })
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
        self.fetch_pem_cert(&url, "VCEK").await
    }

    /// The intermediate and root certificates for this processor, issuer chain order.
    /// Error cases:
    /// - the service does not answer, or answers without both certificates
    pub async fn issuer_chain(&self) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        let url = format!("{}/vcek/v1/{}/cert_chain", self.base.trim_end_matches('/'), self.processor);
        let pem = self.fetch(&url).await?;
        let certs = pem_certificates(&pem)?;
        let [intermediate, root] = certs.as_slice() else {
            bail!("cert chain answered with {} certificates, want 2", certs.len());
        };
        Ok((intermediate.clone(), root.clone()))
    }

    async fn fetch_pem_cert(&self, url: &str, what: &str) -> anyhow::Result<Vec<u8>> {
        let pem = self.fetch(url).await?;
        pem_certificates(&pem)?
            .into_iter()
            .next()
            .with_context(|| format!("collateral answered without a {what} certificate"))
    }

    async fn fetch(&self, url: &str) -> anyhow::Result<String> {
        let mut last = None;
        for _ in 0..FETCH_ATTEMPTS {
            match self.client.get(url).send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        return response.text().await.context("reading the collateral answer");
                    }
                    last = Some(format!("{} answered {}", url, response.status()));
                }
                Err(err) => last = Some(format!("fetching {url}: {err}")),
            }
        }
        Err(anyhow::anyhow!(last.expect("at least one attempt")))
    }
}

/// Parses a DER certificate.
/// Error cases:
/// - the bytes are not a certificate
fn parse_cert(der: &[u8]) -> anyhow::Result<X509Certificate<'_>> {
    let (_, cert) = X509Certificate::from_der(der).map_err(|err| anyhow::anyhow!("parsing a certificate: {err:?}"))?;
    Ok(cert)
}

/// Parses the first PEM certificate in `pem` to DER.
/// Error cases:
/// - the PEM holds no certificate
fn parse_pem_cert(pem: &str) -> anyhow::Result<Vec<u8>> {
    pem_certificates(pem)?.into_iter().next().context("PEM holds no certificate")
}

/// Every PEM certificate in `pem`, as DER.
/// Error cases:
/// - a PEM block is malformed
fn pem_certificates(pem: &str) -> anyhow::Result<Vec<Vec<u8>>> {
    x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes())
        .map(|block| {
            let block = block.map_err(|err| anyhow::anyhow!("malformed PEM: {err}"))?;
            Ok((block.label, block.contents))
        })
        .filter(|block| block.as_ref().is_ok_and(|(label, _)| label == "CERTIFICATE"))
        .map(|block| block.map(|(_, der)| der))
        .collect()
}

/// The vendor roots compiled into the binary. Rotation adds a file here; it never removes one.
const VENDOR_ROOT_PEMS: &[&str] =
    &[include_str!("roots/ark-milan.pem"), include_str!("roots/ark-genoa.pem"), include_str!("roots/ark-turin.pem")];
