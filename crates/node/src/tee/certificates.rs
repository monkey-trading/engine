//! The member's certificate on the host's data disk, sealed to this release apart from the keys
//! (`tee::release_file`). The keys move to each daily turn's member, and every member serves its own
//! registered hostname under the same TLS key, so a certificate belongs to the member whose hostname
//! it names: no handover carries this store, and a member's next turn finds the certificate it was
//! issued and orders nothing while that is valid (`tls::acme`). Like the delivered secrets, the file
//! is not bound to the vault's generation, which moves at every turn: a certificate signs nothing.
//!
//! The store holds the newest certificate, with the domain, the TLS key, and the ACME directory it
//! was issued for, and serves it only when all three are the ones asked for. A new issue replaces it,
//! since the enclave holds one TLS key at a time.
//!
//! The host holds the file and bounds what each thing it can do with it achieves:
//! - deleting it makes the next start order a certificate, under the CA's rate limits;
//! - restoring an older copy gives an older certificate this release was issued, which the
//!   certificate task serves only for the key it certifies, renewing it at once inside its renewal
//!   window;
//! - altering it, or giving another release's file its name, fails authenticated decryption, and the
//!   node refuses to start (`sealed_certificates_unreadable`).

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{Context, ensure};
use rustls::pki_types::CertificateDer;
use serde::{Deserialize, Serialize};

use crate::tee::release_file::{Kind, ReleaseFile};
use crate::tee::sealing::Sealer;

/// The certificates' file, whose tag opens neither the keys nor the secrets as a certificate.
const CERTIFICATES: Kind =
    Kind { tag: b"engine.sealed-certificates.v1", suffix: "certificates.json", what: "certificates" };

/// A certificate chain a CA issued, and what for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCertificate {
    /// The API domain it names.
    domain: String,
    /// The SHA-256 of the certified TLS key's DER SubjectPublicKeyInfo, hex.
    tls_key_sha256: String,
    /// The ACME directory whose CA issued it.
    directory_url: String,
    /// DER, hex, leaf first.
    chain: Vec<String>,
}

/// This release's sealed certificate. Only the certificate task writes it after startup, so the
/// lock is never contended; it keeps the file and `stored` in step.
pub(crate) struct CertificateStore {
    file: ReleaseFile,
    stored: Mutex<Option<StoredCertificate>>,
}

impl CertificateStore {
    /// The store for this release in `dir`, this code running the config whose SHA-256 is
    /// `config_sha256`: empty when it has stored nothing there.
    /// Error cases:
    /// - the platform cannot report
    /// - the file cannot be read, is not a sealed file this build reads, names another release, or
    ///   does not decrypt to a certificate
    pub fn open(dir: &Path, sealer: Box<dyn Sealer>, config_sha256: [u8; 32]) -> anyhow::Result<Self> {
        let file = ReleaseFile::new(dir, CERTIFICATES, sealer, config_sha256)?;
        let stored = file
            .read()?
            .map(|plaintext| serde_json::from_slice(&plaintext).context("the sealed certificate does not parse"))
            .transpose()?;
        Ok(Self { file, stored: Mutex::new(stored) })
    }

    /// The chain `directory_url`'s CA issued for `domain` and the TLS key whose DER
    /// SubjectPublicKeyInfo hashes to `tls_key_sha256`, if that is the one stored.
    /// Error cases:
    /// - a certificate is not hex, or the chain is empty
    pub fn chain_for(
        &self,
        domain: &str,
        tls_key_sha256: &[u8; 32],
        directory_url: &str,
    ) -> anyhow::Result<Option<Vec<CertificateDer<'static>>>> {
        let stored = self.lock();
        let Some(stored) = stored.as_ref().filter(|stored| {
            stored.domain == domain
                && stored.tls_key_sha256 == hex::encode(tls_key_sha256)
                && stored.directory_url == directory_url
        }) else {
            return Ok(None);
        };
        ensure!(!stored.chain.is_empty(), "the sealed certificate chain is empty");
        let chain = stored
            .chain
            .iter()
            .map(|der| hex::decode(der).map(CertificateDer::from))
            .collect::<Result<Vec<_>, _>>()
            .context("a sealed certificate is not hex")?;
        Ok(Some(chain))
    }

    /// Seals `chain`, which `directory_url`'s CA issued for `domain` and the TLS key whose DER
    /// SubjectPublicKeyInfo hashes to `tls_key_sha256`, in place of the stored one; on failure the
    /// file and the store keep what they held.
    /// Error cases:
    /// - the file cannot be written and synced
    pub fn replace(
        &self,
        domain: &str,
        tls_key_sha256: &[u8; 32],
        directory_url: &str,
        chain: &[CertificateDer<'_>],
    ) -> anyhow::Result<()> {
        let certificate = StoredCertificate {
            domain: domain.to_owned(),
            tls_key_sha256: hex::encode(tls_key_sha256),
            directory_url: directory_url.to_owned(),
            chain: chain.iter().map(hex::encode).collect(),
        };
        let mut stored = self.lock();
        self.file.write(serde_json::to_vec(&certificate).context("encoding the certificate")?)?;
        *stored = Some(certificate);
        Ok(())
    }

    /// The release the store is sealed to, for tests to find its file.
    #[cfg(test)]
    pub(crate) fn release(&self) -> crate::tee::sealing::Release {
        self.file.release()
    }

    /// The stored certificate, whether or not a panic interrupted an earlier holder: `replace` swaps
    /// it whole.
    fn lock(&self) -> MutexGuard<'_, Option<StoredCertificate>> {
        self.stored.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The file `release`'s certificate is sealed in, for tests.
#[cfg(test)]
pub(crate) fn file_for(dir: &Path, release: &crate::tee::sealing::Release) -> std::path::PathBuf {
    crate::tee::release_file::file_for(dir, release, &CERTIFICATES)
}

#[cfg(test)]
mod tests;
