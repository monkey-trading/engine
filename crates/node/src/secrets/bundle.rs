//! What an operator delivers: a version and the named secrets, under the operator's signature. The
//! signature covers the deployment, the version, and the contents, and not the release, so it still
//! proves who chose these secrets once an approved upgrade has moved them to the next release.
//! Errors here never quote what failed to parse, since a mistyped file can put a value where a
//! parser would repeat it.

use std::fmt;
use std::path::Path;

use anyhow::{Context, bail};
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use serde::{Deserialize, Serialize};
use sha3::{Digest as _, Keccak256};

use crate::config::{NamedSecret, SecretName, SecretUse, Values, WebhookUrl};
use crate::log::CheckpointDomain;

/// Domain tag of what the operator signs.
const SIGNED_TAG: &[u8] = b"engine.secrets.bundle.v1";

/// A version and the secrets it holds. `Debug` prints the names alone.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bundle {
    /// Rises with each rotation; the enclave refuses anything older than it holds.
    pub version: u64,
    pub secrets: Values,
}

/// A bundle and the operator's signature over it, as it is delivered, sealed, and moved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SignedBundle {
    pub bundle: Bundle,
    /// 65-byte `r || s || v`, hex.
    signature: String,
}

impl fmt::Debug for Bundle {
    /// The version and the names, without their values.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bundle")
            .field("version", &self.version)
            .field("secrets", &self.secrets.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Bundle {
    /// The bundle in the operator's TOML file: `version = 1` and a `[secrets]` table of names and
    /// values.
    /// Error cases:
    /// - the file cannot be read, or is not that shape, reported by line without its text
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).map_err(|e: toml::de::Error| {
            let line = e.span().map_or(0, |span| text[..span.start].matches('\n').count() + 1);
            anyhow::anyhow!(
                "{} line {line} is not a secrets bundle: `version` as an integer and a [secrets] table of lowercase \
                 names and string values, with no other keys",
                path.display()
            )
        })
    }

    /// What the operator's key signs: the tag, the deployment, the version, and each name and value
    /// in order, each with its length, so no two bundles share an encoding.
    pub fn digest(&self, domain: CheckpointDomain) -> B256 {
        let mut hash = Keccak256::new();
        hash.update(SIGNED_TAG);
        hash.update(domain.chain_id.to_be_bytes());
        hash.update(domain.vault.0);
        hash.update(self.version.to_be_bytes());
        for (name, value) in &self.secrets {
            for part in [name.to_string().as_bytes(), value.expose().as_bytes()] {
                hash.update((part.len() as u64).to_be_bytes());
                hash.update(part);
            }
        }
        B256::from(<[u8; 32]>::from(hash.finalize()))
    }

    /// This bundle under `key`'s signature.
    pub fn sign(self, domain: CheckpointDomain, key: &SigningKey) -> SignedBundle {
        let signature = format!("0x{}", hex::encode(eip712::sign(key, &self.digest(domain))));
        SignedBundle { bundle: self, signature }
    }

    /// Checks a bundle being delivered against the fields that read it, before it is sealed or
    /// encrypted.
    /// Error cases:
    /// - it names a secret no field reads, which is a typo that would leave a field without its own
    /// - see `check_values`
    pub fn check(&self, named: &[NamedSecret]) -> anyhow::Result<()> {
        if let Some(name) = self.secrets.keys().find(|name| !named.iter().any(|field| field.name == **name)) {
            bail!("the bundle holds {name}, which no field of this config reads");
        }
        self.check_values(named)
    }

    /// Checks what each field reads from this bundle. A bundle a predecessor's config chose may hold
    /// more than this config reads: it stays whole, since the operator's signature covers it all.
    /// Error cases:
    /// - a value a field reads is malformed (see `SecretValue::check`), or a webhook's is not an
    ///   acceptable URL
    pub fn check_values(&self, named: &[NamedSecret]) -> anyhow::Result<()> {
        for field in named {
            let Some(value) = self.secrets.get(&field.name) else { continue };
            value.check(&field.name)?;
            if field.usage == SecretUse::WebhookUrl {
                WebhookUrl::parse(&field.field, value.expose())?;
            }
        }
        Ok(())
    }

    /// The names the config reads that this bundle lacks, each once.
    pub fn missing(&self, named: &[NamedSecret]) -> Vec<SecretName> {
        missing_from(&self.secrets, named)
    }
}

impl SignedBundle {
    /// The key that signed this bundle for `domain`.
    /// Error cases:
    /// - the signature is malformed or recovers no key
    pub fn signer(&self, domain: CheckpointDomain) -> anyhow::Result<AccountId> {
        let signature: Signature =
            self.signature.parse().map_err(|_| anyhow::anyhow!("the bundle's signature is malformed"))?;
        eip712::recover(&self.bundle.digest(domain), &signature).context("the bundle's signature recovers no key")
    }

    /// The encoding the delivery encrypts and the seal holds.
    /// Error cases:
    /// - encoding fails
    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|_| anyhow::anyhow!("encoding the bundle failed"))
    }

    /// Decodes `to_bytes`.
    /// Error cases:
    /// - the bytes are not a signed bundle
    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("the decrypted bundle does not parse"))
    }
}

/// The names the config reads that `values` lacks, each once.
pub(crate) fn missing_from(values: &Values, named: &[NamedSecret]) -> Vec<SecretName> {
    let mut missing: Vec<SecretName> =
        named.iter().filter(|field| !values.contains_key(&field.name)).map(|field| field.name.clone()).collect();
    missing.sort();
    missing.dedup();
    missing
}

#[cfg(test)]
mod tests;
