//! Polling the node's `/v1/attestation` and verifying it with the native verifier. Only the
//! attestation is trusted, never the connection: the quote's report data binds the TLS key hash it
//! names, so a connection that serves a bad attestation, or none, can only withhold keys.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use reqwest::{Client, Url};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring::default_provider;
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

use crate::ct_watch::config::WatchedNode;
use crate::tee::{Attestation, MOCK_QUOTE_MARKER};
use crate::verify::{CollateralUnavailable, Options, ReleaseManifest, served_bundle, verify_attestation};

/// An attestation is a few KiB of hex.
const MAX_ATTESTATION_BYTES: usize = 1024 * 1024;
/// A whole fetch, connection and body included.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// What a poll of the node came to.
#[derive(Debug)]
pub(super) enum Poll {
    /// The SHA-256 of the TLS key a verified attestation names.
    Verified([u8; 32]),
    /// The node, or the vendor service the quote's chain comes from, did not answer; no key can be
    /// learned, none is wrongly named.
    Unreachable(anyhow::Error),
    /// The node served an attestation that does not verify.
    Rejected(anyhow::Error),
}

/// The node's attestation endpoint and what verifies what it serves.
pub(super) struct AttestationSource {
    http: Client,
    url: Url,
    release: ReleaseManifest,
    processor: String,
    kds: String,
    root_pem: Option<String>,
    allow_mock: bool,
}

/// Accepts any server certificate, checking only its handshake signatures. The attestation, not
/// the connection, is what the watcher trusts.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    /// Accepts the certificate.
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    /// Checks the handshake signature with the certificate's key.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    /// Checks the handshake signature with the certificate's key.
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    /// Every scheme the provider verifies.
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

impl AttestationSource {
    /// Loads the release manifest and extra root named in `node` (whose URL the config has validated),
    /// and builds the HTTP client.
    /// Error cases:
    /// - the URL, manifest or root cannot be read or parsed, or the client cannot be built
    pub(super) fn new(node: &WatchedNode, allow_mock: bool) -> anyhow::Result<Self> {
        let mut url = Url::parse(&node.url).context("node.url")?;
        url.path_segments_mut()
            .map_err(|()| anyhow::anyhow!("node.url cannot have path segments"))?
            .pop_if_empty()
            .extend(["v1", "attestation"]);
        let provider = Arc::new(default_provider());
        let tls = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth();
        let http = Client::builder()
            .user_agent(concat!("engine-ct-watch/", env!("CARGO_PKG_VERSION")))
            .tls_backend_preconfigured(tls)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .context("building the node HTTP client")?;
        let release = ReleaseManifest::load(&node.release)?;
        let root_pem = node.root.as_deref().map(read_pem).transpose()?;
        Ok(Self { http, url, release, processor: node.processor.clone(), kds: node.kds.clone(), root_pem, allow_mock })
    }

    /// Fetches the node's attestation and verifies it against the release.
    pub(super) async fn poll(&self) -> Poll {
        let body = match self.fetch().await {
            Ok(body) => body,
            Err(e) => return Poll::Unreachable(e),
        };
        let attestation: Attestation = match serde_json::from_slice(&body) {
            Ok(attestation) => attestation,
            Err(e) => return Poll::Rejected(anyhow::Error::new(e).context("the node's answer is not an attestation")),
        };
        classify(self.verify(&attestation).await)
    }

    /// The body the node serves for its attestation.
    /// Error cases:
    /// - the request fails or times out, the status is not success, or the body is over the bound
    async fn fetch(&self) -> anyhow::Result<Vec<u8>> {
        let exchange = async {
            let mut response = self.http.get(self.url.clone()).send().await?.error_for_status()?;
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                ensure!(body.len().saturating_add(chunk.len()) <= MAX_ATTESTATION_BYTES, "the attestation is too long");
                body.extend_from_slice(&chunk);
            }
            anyhow::Ok(body)
        };
        tokio::time::timeout(FETCH_TIMEOUT, exchange).await.context("the attestation request timed out")?
    }

    /// The TLS key hash `attestation` proves: from a verified hardware quote, or with `allow_mock`
    /// from a mock one whose report data binds the served bundle.
    /// Error cases:
    /// - the quote does not verify against the release, or is a mock one the watcher does not allow
    async fn verify(&self, attestation: &Attestation) -> anyhow::Result<[u8; 32]> {
        if attestation.provider == "mock" && self.allow_mock {
            return check_mock(attestation);
        }
        let options = Options { processor: &self.processor, kds_base: &self.kds, root_pem: self.root_pem.as_deref() };
        Ok(verify_attestation(attestation, &self.release, &options).await?.tls_public_key_sha256)
    }
}

/// What a verification came to: a vendor outage says nothing about the attestation, any other failure
/// is a rejection.
pub(super) fn classify(verified: anyhow::Result<[u8; 32]>) -> Poll {
    match verified {
        Ok(key) => Poll::Verified(key),
        Err(e) if e.downcast_ref::<CollateralUnavailable>().is_some() => Poll::Unreachable(e),
        Err(e) => Poll::Rejected(e),
    }
}

/// The TLS key hash of a mock attestation: its quote is the mock marker and the report data, which
/// binds the served bundle. Dev only; nothing proves the bundle came from an enclave.
/// Error cases:
/// - the quote is not the mock marker and the report data, or the bundle is not bound by it
fn check_mock(attestation: &Attestation) -> anyhow::Result<[u8; 32]> {
    let (bundle, report_data) = served_bundle(attestation)?;
    let quote = hex::decode(attestation.quote.strip_prefix("0x").unwrap_or(&attestation.quote)).context("quote")?;
    ensure!(
        quote == [MOCK_QUOTE_MARKER, &report_data[..]].concat(),
        "the quote is not a mock quote for the report data"
    );
    Ok(bundle.tls_public_key_sha256)
}

/// Reads a PEM root certificate file.
/// Error cases:
/// - the file cannot be read
fn read_pem(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}
