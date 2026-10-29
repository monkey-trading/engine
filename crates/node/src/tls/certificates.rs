//! The certificates the API's key serves. The certificate task owns the senders and swaps in a CA
//! certificate, or the challenge certificate of a TLS-ALPN-01 validation; each handshake reads the
//! latest value. A `watch` channel fits where a request to an owning task cannot: rustls resolves a
//! certificate synchronously inside the handshake.

use std::sync::Arc;

use anyhow::{Context, ensure};
use rcgen::{CertificateParams, CustomExtension, DistinguishedName, KeyPair};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::{CertifiedKey, SigningKey};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use webpki::EndEntityCert;

/// The API's key, and the slots for the certificates it serves. Held only in enclave memory.
pub struct TlsKey {
    provider: Arc<CryptoProvider>,
    /// Signs certificate requests, proving the key to the CA.
    key_pair: KeyPair,
    /// The same key, as rustls signs handshakes with it.
    signer: Arc<dyn SigningKey>,
    public_key_sha256: [u8; 32],
    served: watch::Sender<Arc<CertifiedKey>>,
    challenge: watch::Sender<Option<Challenge>>,
}

/// A TLS-ALPN-01 challenge being answered: the name it validates, and its certificate.
#[derive(Debug)]
pub(super) struct Challenge {
    domain: String,
    certificate: Arc<CertifiedKey>,
}

/// Serves a challenge certificate until dropped, so an attempt that ends early, by error or
/// cancellation, leaves none behind.
pub struct Answer<'a>(&'a watch::Sender<Option<Challenge>>);

/// Resolves the API's handshakes to the certificate served now.
#[derive(Debug)]
pub(super) struct Served(pub(super) watch::Receiver<Arc<CertifiedKey>>);

/// Resolves a validation's handshake to the challenge certificate for its server name, if one is
/// being answered.
#[derive(Debug)]
pub(super) struct Answering(pub(super) watch::Receiver<Option<Challenge>>);

impl ResolvesServerCert for Served {
    /// The certificate served now: self-signed, or the latest from the CA.
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.borrow().clone())
    }
}

impl ResolvesServerCert for Answering {
    /// The challenge certificate, when the hello names the domain being validated.
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let challenge = self.0.borrow();
        let challenge = challenge.as_ref()?;
        let named = hello.server_name().is_some_and(|name| name.eq_ignore_ascii_case(&challenge.domain));
        named.then(|| challenge.certificate.clone())
    }
}

impl Drop for Answer<'_> {
    /// Stops answering the challenge.
    fn drop(&mut self) {
        self.0.send_replace(None);
    }
}

impl TlsKey {
    /// Wraps a fresh key and the senders of the certificates its server configs resolve.
    pub(super) fn new(
        provider: Arc<CryptoProvider>,
        key_pair: KeyPair,
        signer: Arc<dyn SigningKey>,
        public_key_sha256: [u8; 32],
        served: watch::Sender<Arc<CertifiedKey>>,
        challenge: watch::Sender<Option<Challenge>>,
    ) -> Self {
        Self { provider, key_pair, signer, public_key_sha256, served, challenge }
    }

    /// The key pair, for tests to have a CA certify it.
    #[cfg(test)]
    pub(crate) fn key_pair(&self) -> &KeyPair {
        &self.key_pair
    }

    /// A certificate request for `domain` alone, signed by the key: what an ACME order is
    /// finalized with.
    /// Error cases:
    /// - `domain` is not a valid name, or the request cannot be signed
    pub fn certificate_request(&self, domain: &str) -> anyhow::Result<Vec<u8>> {
        let mut params = CertificateParams::new(vec![domain.to_owned()]).context("certificate request parameters")?;
        params.distinguished_name = DistinguishedName::new();
        let request = params.serialize_request(&self.key_pair).context("signing the certificate request")?;
        Ok(request.der().to_vec())
    }

    /// Serves `chain` from the next handshake on.
    /// Error cases:
    /// - the chain is empty, or its first certificate does not parse or is not for this key
    pub fn install(&self, chain: Vec<CertificateDer<'static>>) -> anyhow::Result<()> {
        let leaf = chain.first().context("the CA returned no certificate")?;
        let leaf = EndEntityCert::try_from(leaf).map_err(|e| anyhow::anyhow!("parsing the certificate: {e}"))?;
        let key_sha256: [u8; 32] = Sha256::digest(leaf.subject_public_key_info()).into();
        ensure!(key_sha256 == self.public_key_sha256, "the certificate is not for the attested key");
        self.served.send_replace(Arc::new(CertifiedKey::new(chain, self.signer.clone())));
        Ok(())
    }

    /// Answers TLS-ALPN-01 for `domain` until the returned guard drops, with a certificate under a
    /// fresh key that carries `digest`, the SHA-256 of the key authorization (RFC 8737 §3).
    /// Error cases:
    /// - the challenge key or certificate cannot be made
    pub fn answer(&self, domain: &str, digest: [u8; 32]) -> anyhow::Result<Answer<'_>> {
        let key_pair = KeyPair::generate().context("generating the challenge key")?;
        let mut params = CertificateParams::new(vec![domain.to_owned()]).context("challenge certificate parameters")?;
        params.custom_extensions = vec![CustomExtension::new_acme_identifier(&digest)];
        let certificate = params.self_signed(&key_pair).context("signing the challenge certificate")?;
        let certificate = CertifiedKey::new(vec![certificate.der().clone()], load_key(&self.provider, &key_pair)?);
        self.challenge.send_replace(Some(Challenge { domain: domain.to_owned(), certificate: Arc::new(certificate) }));
        Ok(Answer(&self.challenge))
    }
}

/// `key_pair` as rustls signs with it.
/// Error cases:
/// - the provider does not take the key
pub(super) fn load_key(provider: &CryptoProvider, key_pair: &KeyPair) -> anyhow::Result<Arc<dyn SigningKey>> {
    provider
        .key_provider
        .load_private_key(PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into())
        .context("loading the TLS key")
}
