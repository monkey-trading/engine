//! The enclave's keys and what belongs with them: what a restart keeps and an upgrade hands to the
//! next release. The signing key is the vault's signer and the attested address; the TLS key is the
//! attested key the API serves, with the ACME account it orders certificates under, which moves with
//! it so a CAA `accounturi` can name the enclave's own account across every handover. Delivered
//! secrets do not belong with them (`tee::secrets`): the keys move to an elected operator, while its
//! vendor accounts and provider keys are its own. Nor do certificates (`tee::certificates`): each
//! daily turn's member serves its own hostname, so a certificate stays with the member it names.

use std::fmt;

use anyhow::Context;
use k256::ecdsa::SigningKey;
use k256::elliptic_curve::Generate;
use rcgen::KeyPair;
use serde::{Deserialize, Serialize};

use crate::tee::EnclaveKey;

/// The enclave's keys. Secrets are hex; `Debug` prints none of them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeySet {
    /// The secp256k1 signing key's 32-byte secret.
    signing_key: String,
    /// The API's TLS key, PKCS#8 DER.
    tls_key: String,
    /// The ACME account, once registered.
    #[serde(default)]
    pub acme_account: Option<AcmeAccount>,
    /// A certificate an earlier release sealed with the keys. It is still read, so those keys open,
    /// but never served or handed over: certificates stay in each member's store.
    #[serde(default)]
    pub certificate: Option<SealedCertificate>,
    /// The final checkpoint of the release that handed these keys over, which this release's log
    /// must contain before it serves.
    #[serde(default)]
    pub predecessor_head: Option<Head>,
    /// Every log segment past the successor's tip the handover carries, newest last, with the
    /// listings the predecessor sealed them under: inside the ciphertext, they bind the raw
    /// segment frames that follow the keys.
    #[serde(default)]
    pub handover_tails: Vec<crate::log::SegmentInfo>,
    /// The entries past a following successor's tip a tail handover carries in place of whole
    /// segments: inside the ciphertext, the listing binds the raw frames that follow the keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handover_tail: Option<crate::handover::tail::TailListing>,
    /// The head of the log the last boundary's check verified (`log_check`), which a restart restores
    /// and an approved upgrade carries to the next release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_check: Option<crate::log_check::VerifiedHead>,
}

/// An ACME account and the directory it is registered with.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcmeAccount {
    pub directory_url: String,
    /// The ACME client's credentials, which carry the account key.
    pub credentials: serde_json::Value,
}

/// A certificate chain and the ACME directory that issued it, as earlier releases sealed it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SealedCertificate {
    pub directory_url: String,
    /// DER, hex, leaf first.
    pub chain: Vec<String>,
}

/// A checkpoint: the last command of a log and the state root after it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Head {
    pub seq: u64,
    /// 0x-prefixed keccak-256 state root, as the log writes it.
    pub root: String,
}

impl fmt::Debug for KeySet {
    /// What the set holds, without its secrets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeySet")
            .field("acme_account", &self.acme_account.as_ref().map(|account| &account.directory_url))
            .field("certificate", &self.certificate.as_ref().map(|certificate| &certificate.directory_url))
            .field("predecessor_head", &self.predecessor_head)
            .field("log_check", &self.log_check)
            .finish_non_exhaustive()
    }
}

impl KeySet {
    /// A fresh signing key and TLS key from the enclave's RNG, with no account or certificate yet.
    /// Error cases:
    /// - the TLS key cannot be generated
    pub fn generate() -> anyhow::Result<Self> {
        let tls = KeyPair::generate().context("generating the TLS key")?;
        Ok(Self {
            signing_key: hex::encode(SigningKey::generate().to_bytes()),
            tls_key: hex::encode(tls.serialize_der()),
            acme_account: None,
            certificate: None,
            predecessor_head: None,
            handover_tails: Vec::new(),
            handover_tail: None,
            log_check: None,
        })
    }

    /// A set holding `key` as the signing key, with a fresh TLS key: tests that replay a log the
    /// shared test key sealed a checkpoint under seal it, so a node's sealed keys open as that key.
    /// Error cases:
    /// - the TLS key cannot be generated
    #[cfg(test)]
    pub(crate) fn for_key(key: &EnclaveKey) -> anyhow::Result<Self> {
        let mut set = Self::generate()?;
        set.signing_key = hex::encode(key.key.to_bytes());
        Ok(set)
    }

    /// The signing key.
    /// Error cases:
    /// - the secret is not 32 bytes of hex or not a valid secp256k1 scalar
    pub fn enclave_key(&self) -> anyhow::Result<EnclaveKey> {
        let secret: [u8; 32] = hex::decode(&self.signing_key)
            .context("the sealed signing key is not hex")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("the sealed signing key is not 32 bytes"))?;
        EnclaveKey::from_secret(&secret)
    }

    /// The TLS key.
    /// Error cases:
    /// - the key is not hex PKCS#8 DER of a key the TLS stack supports
    pub fn tls_key(&self) -> anyhow::Result<KeyPair> {
        let der = hex::decode(&self.tls_key).context("the sealed TLS key is not hex")?;
        KeyPair::try_from(der).context("the sealed TLS key does not load")
    }

    /// Records `chain` as an earlier release sealed its certificate with the keys, for tests.
    #[cfg(test)]
    pub(crate) fn set_certificate(&mut self, directory_url: &str, chain: &[rustls::pki_types::CertificateDer<'_>]) {
        let chain = chain.iter().map(hex::encode).collect();
        self.certificate = Some(SealedCertificate { directory_url: directory_url.to_owned(), chain });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::PublicKeyData;
    use rustls::pki_types::CertificateDer;

    #[test]
    fn a_set_round_trips_its_keys_and_prints_no_secret() {
        let mut keys = KeySet::generate().unwrap();
        keys.set_certificate("https://ca.test/dir", &[CertificateDer::from(vec![1, 2, 3])]);
        keys.predecessor_head = Some(Head { seq: 7, root: format!("0x{}", "ab".repeat(32)) });
        let restored: KeySet = serde_json::from_slice(&serde_json::to_vec(&keys).unwrap()).unwrap();
        assert_eq!(restored, keys);
        assert_eq!(restored.enclave_key().unwrap().address(), keys.enclave_key().unwrap().address());
        assert_eq!(
            restored.tls_key().unwrap().subject_public_key_info(),
            keys.tls_key().unwrap().subject_public_key_info()
        );

        let printed = format!("{keys:?}");
        assert!(!printed.contains(&keys.signing_key) && !printed.contains(&keys.tls_key), "{printed}");
    }

    #[test]
    fn a_verified_head_round_trips_and_a_set_without_one_encodes_as_before() {
        let mut keys = KeySet::generate().unwrap();
        let encoded: serde_json::Value = serde_json::to_value(&keys).unwrap();
        assert!(encoded.get("log_check").is_none(), "a set with no verified head names one: {encoded}");
        let seq = 7;
        let (link, root) = (alloy::primitives::B256::repeat_byte(1), alloy::primitives::B256::repeat_byte(2));
        keys.log_check = Some(crate::log_check::VerifiedHead { seq, link, root });
        let restored: KeySet = serde_json::from_slice(&serde_json::to_vec(&keys).unwrap()).unwrap();
        assert_eq!(restored.log_check, keys.log_check);
    }

    #[test]
    fn two_generated_sets_share_no_key() {
        let (a, b) = (KeySet::generate().unwrap(), KeySet::generate().unwrap());
        assert_ne!(a.enclave_key().unwrap().address(), b.enclave_key().unwrap().address());
        assert_ne!(a.tls_key, b.tls_key);
    }

    #[test]
    fn a_damaged_set_refuses_to_load_its_keys() {
        let keys = KeySet::generate().unwrap();
        for (what, change) in [
            ("short signing key", Box::new(|k: &mut KeySet| k.signing_key.truncate(62)) as Box<dyn Fn(&mut KeySet)>),
            ("zero signing key", Box::new(|k: &mut KeySet| k.signing_key = "00".repeat(32))),
            ("non-hex TLS key", Box::new(|k: &mut KeySet| k.tls_key = "zz".into())),
        ] {
            let mut damaged = keys.clone();
            change(&mut damaged);
            assert!(damaged.enclave_key().is_err() || damaged.tls_key().is_err(), "{what} loaded");
        }
    }
}
