//! What `/v1/attestation` serves, and the bundle its quote binds. The node builds both; a
//! verifier reads them back, so the two share this one definition.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The attestation bundle's domain tag, which names its layout version; a new layout takes a new
/// tag.
const BUNDLE_TAG: &[u8] = b"engine.attestation.v1";

/// What a mock quote starts with, before the report data.
pub const MOCK_QUOTE_MARKER: &[u8] = b"MOCK-ATTESTATION-NOT-A-TEE:";

/// What an attestation vouches for. Its encoding is the domain tag followed by each field, at fixed
/// widths and in this order; report data bytes 0..32 are the SHA-256 of that encoding, and bytes
/// 32..64 are reserved as zero.
pub struct AttestationBundle {
    /// The enclave signing key's EVM address.
    pub enclave_address: [u8; 20],
    /// SHA-256 of the config file the node loaded.
    pub config_sha256: [u8; 32],
    /// SHA-256 of the API certificate's DER SubjectPublicKeyInfo.
    pub tls_public_key_sha256: [u8; 32],
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Attestation {
    pub provider: String,
    pub enclave_address: String,
    /// SHA-256 of the config file the node loaded, hex.
    pub config_sha256: String,
    /// SHA-256 of the API certificate's DER SubjectPublicKeyInfo, hex.
    pub tls_public_key_sha256: String,
    /// The bundle's encoding, hex: a verifier checks that its SHA-256 is report data bytes 0..32
    /// and reads the fields above from it.
    pub bundle: String,
    pub report_data: String,
    /// Hardware quote (TDX quote or SEV-SNP report), hex.
    pub quote: String,
}

impl AttestationBundle {
    /// The bundle's bytes: `BUNDLE_TAG`, the enclave address, the config hash, and the TLS key hash.
    pub fn encode(&self) -> Vec<u8> {
        [BUNDLE_TAG, &self.enclave_address[..], &self.config_sha256[..], &self.tls_public_key_sha256[..]].concat()
    }

    /// Parses `bytes` as an encoded bundle; `None` when the bytes are not one.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(BUNDLE_TAG)?;
        let (enclave_address, rest) = rest.split_at_checked(20)?;
        let (config_sha256, tls_public_key_sha256) = rest.split_at_checked(32)?;
        if tls_public_key_sha256.len() != 32 {
            return None;
        }
        Some(Self {
            enclave_address: enclave_address.try_into().ok()?,
            config_sha256: config_sha256.try_into().ok()?,
            tls_public_key_sha256: tls_public_key_sha256.try_into().ok()?,
        })
    }

    /// The 64 bytes a quote binds: the SHA-256 of the encoding, then 32 reserved zero bytes.
    pub fn report_data(&self) -> [u8; 64] {
        let mut report_data = [0u8; 64];
        report_data[..32].copy_from_slice(&Sha256::digest(self.encode()));
        report_data
    }
}
