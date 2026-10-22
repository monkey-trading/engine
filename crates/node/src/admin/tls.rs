//! The TLS connection `engine admin` makes to a node: TLS 1.3, accepted only when the server's key
//! hashes to the pin the operator gives, the SHA-256 of its DER SubjectPublicKeyInfo that the
//! node's attestation carries. The node's certificate is self-signed, so the pin, not a CA, is what
//! makes it trustworthy.

use std::sync::Arc;

use anyhow::Context;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring::default_provider;
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::version::TLS13;
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use webpki::EndEntityCert;

/// Accepts a server certificate only when its key hashes to `pin`.
#[derive(Debug)]
struct PinnedKey {
    pin: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedKey {
    /// Accepts the certificate when the SHA-256 of its SubjectPublicKeyInfo is the pin.
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = EndEntityCert::try_from(end_entity).map_err(|e| rustls::Error::General(e.to_string()))?;
        let hash: [u8; 32] = Sha256::digest(cert.subject_public_key_info()).into();
        if hash != self.pin {
            return Err(rustls::Error::General("the node's TLS key is not the pinned one".into()));
        }
        Ok(ServerCertVerified::assertion())
    }

    /// Checks the handshake signature with the certificate's key.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    /// Checks the handshake signature with the certificate's key.
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    /// Every scheme the provider verifies.
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Parses a pin: the TLS key hash in the node's attested bundle, 32 bytes in hex.
/// Error cases:
/// - not 32 bytes of hex, with or without `0x`
pub(crate) fn parse_pin(text: &str) -> Result<[u8; 32], String> {
    hex::decode(text.trim_start_matches("0x"))
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| "a pin is the TLS key hash in the node's attested bundle: 32 bytes in hex".to_owned())
}

/// A TLS 1.3 connection to `host`:`port` whose server key hashes to `pin`.
/// Error cases:
/// - `host` is not a hostname or an IP address
/// - the connection fails, or the handshake does, including when the key is not the pinned one
pub(crate) async fn connect(host: &str, port: u16, pin: [u8; 32]) -> anyhow::Result<TlsStream<TcpStream>> {
    let provider = Arc::new(default_provider());
    let verifier = Arc::new(PinnedKey { pin, provider: provider.clone() });
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])
        .context("TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let name = ServerName::try_from(host.to_owned()).with_context(|| format!("{host} is not a host name"))?;
    let tcp = TcpStream::connect((host, port)).await?;
    Ok(TlsConnector::from(Arc::new(config)).connect(name, tcp).await?)
}
