//! Catalog-only signed write heads; these never acknowledge engine/history state or Work.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Context, ensure};
use engine_core::eip712::{self, B256, Signature};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::authenticated::{CODEC, Codec};
use crate::log::catalog::CatalogRoot;
use crate::tee::EnclaveKey;

pub(crate) const HEAD_FORMAT: u32 = 3;
const MAX_HEAD_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogDomain {
    pub chain_id: u64,
    pub vault: [u8; 20],
    /// Existing independently authenticated deployment/genesis/log identity, never a disk key.
    pub log_id: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogHeadBody {
    pub format: u32,
    pub codec: Codec,
    pub domain: CatalogDomain,
    /// Namespace-scoped monotonic Store version, including internal maintenance advances.
    pub nonce: u64,
    pub catalog: CatalogRoot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogHeadCertificate {
    pub body: CatalogHeadBody,
    pub signer: [u8; 20],
    pub signature: String,
}

#[derive(Clone)]
pub struct CatalogTrust {
    pub domain: CatalogDomain,
    /// Loaded from existing independent attestation/signer pins, never from the certificate.
    pub signers: BTreeSet<[u8; 20]>,
}

#[derive(Clone, Debug)]
pub struct VerifiedCatalogHead(CatalogHeadCertificate);

/// The parent implements fsync/atomic publication and root-pin retention with this callback.
pub trait CatalogHeadPublication: Send + Sync {
    /// Refreshes actual retained-root leases before a bounded collector turn.
    /// Error cases:
    /// - retained metadata cannot be completely verified, so reclamation must wait
    fn prepare_maintenance(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Replaces the fixed latest-head slot only after Store file/directory durability.
    /// Error cases:
    /// - any stage fails; the previous stable certificate and its root pin must remain available
    fn publish(&self, previous: Option<&CatalogHeadCertificate>, next: &CatalogHeadCertificate) -> anyhow::Result<()>;

    /// Reads the bounded latest-head slot without creation, repair, enumeration or writable locks.
    /// Error cases:
    /// - the slot cannot be read or exceeds MAX_HEAD_BYTES
    fn read(&self) -> anyhow::Result<Option<Vec<u8>>>;
}

pub struct CatalogHeadAuthority {
    trust: CatalogTrust,
    key: Arc<EnclaveKey>,
    publication: Arc<dyn CatalogHeadPublication>,
    stable: Option<VerifiedCatalogHead>,
}

impl CatalogHeadCertificate {
    /// Verifies exact format, nonce, domain and signature against independent signer pins.
    /// Error cases:
    /// - any authority field, required root, signature or nonce is invalid
    pub fn verify(&self, trust: &CatalogTrust) -> anyhow::Result<VerifiedCatalogHead> {
        ensure!(
            self.body.format == HEAD_FORMAT && self.body.codec == CODEC && self.body.domain == trust.domain,
            "catalog head domain/format/codec mismatch"
        );
        ensure!(self.body.catalog.root.version == Some(self.body.nonce), "catalog head nonce/version mismatch");
        ensure!(trust.signers.contains(&self.signer) && self.signer != [0; 20], "unknown catalog head signer");
        let signature: Signature = self.signature.parse().context("catalog head signature")?;
        let signer =
            eip712::recover(&B256::from(head_digest(&self.body)?), &signature).context("catalog head recovery")?;
        ensure!(signer.0 == self.signer, "catalog head signature mismatch");
        Ok(VerifiedCatalogHead(self.clone()))
    }
}

impl VerifiedCatalogHead {
    /// Returns only the catalog write head; it has no engine/history acknowledgment semantics.
    pub fn catalog(&self) -> &CatalogRoot {
        &self.0.body.catalog
    }

    /// Returns the authenticated deployment identity.
    pub fn domain(&self) -> &CatalogDomain {
        &self.0.body.domain
    }

    /// Returns the authenticated serialized certificate for retention/publication.
    pub fn certificate(&self) -> &CatalogHeadCertificate {
        &self.0
    }
}

impl CatalogHeadAuthority {
    /// Restores retained version leases before the serialized catalog owner collects old rows.
    /// Error cases:
    /// - the publication owner cannot verify all retained references
    pub(super) fn prepare_maintenance(&self) -> anyhow::Result<()> {
        self.publication.prepare_maintenance()
    }

    /// Uses the node's existing enclave key and independent trust context; no key is persisted.
    /// Error cases:
    /// - the supplied signing key is not independently authorized
    pub fn new(
        trust: CatalogTrust,
        key: Arc<EnclaveKey>,
        publication: Arc<dyn CatalogHeadPublication>,
    ) -> anyhow::Result<Self> {
        ensure!(trust.signers.contains(&key.address()), "catalog signing key is not pinned");
        Ok(Self { trust, key, publication, stable: None })
    }

    /// Returns the independently provisioned catalog deployment identity.
    pub fn domain(&self) -> &CatalogDomain {
        &self.trust.domain
    }

    /// Restores the independently verified stable head before post-preflight writable recovery.
    /// Error cases:
    /// - the retained certificate belongs to another trust domain or signer set
    pub fn restore(&mut self, head: VerifiedCatalogHead) -> anyhow::Result<()> {
        self.stable = Some(head.0.verify(&self.trust)?);
        Ok(())
    }

    /// Signs only after a durable Store commit; updates stable state only after publication ack.
    /// Error cases:
    /// - root/nonce regression, signing verification, or atomic/fsync publication fails
    pub(super) fn publish(&mut self, catalog: CatalogRoot) -> anyhow::Result<VerifiedCatalogHead> {
        let nonce = catalog.root.version.context("catalog head has no committed root")?;
        if let Some(previous) = &self.stable {
            ensure!(previous.catalog().root.namespace == catalog.root.namespace, "catalog head namespace changed");
            ensure!(nonce >= previous.0.body.nonce, "catalog head nonce regressed");
            if nonce == previous.0.body.nonce {
                ensure!(previous.catalog() == &catalog, "same-version catalog head changed");
            }
        }
        let body =
            CatalogHeadBody { format: HEAD_FORMAT, codec: CODEC, domain: self.trust.domain.clone(), nonce, catalog };
        let certificate = CatalogHeadCertificate {
            signer: self.key.address(),
            signature: self.key.sign_digest(head_digest(&body)?),
            body,
        };
        let verified = certificate.verify(&self.trust)?;
        self.publication.publish(self.stable.as_ref().map(|h| &h.0), &certificate)?;
        self.stable = Some(verified.clone());
        Ok(verified)
    }
}

/// Reads a fixed certificate slot without changing storage or accepting a disk-nominated signer.
/// Error cases:
/// - unavailable/oversized/malformed certificate or failed independent authentication
pub fn read_catalog_head(
    source: &dyn CatalogHeadPublication,
    trust: &CatalogTrust,
) -> anyhow::Result<VerifiedCatalogHead> {
    let bytes = source.read()?.context("catalog head unavailable")?;
    ensure!(bytes.len() <= MAX_HEAD_BYTES, "catalog head too large");
    serde_json::from_slice::<CatalogHeadCertificate>(&bytes)?.verify(trust)
}

/// Hashes the canonical typed payload with a distinct catalog-head signature domain.
/// Error cases:
/// - payload encoding fails or exceeds its envelope
pub fn head_digest(body: &CatalogHeadBody) -> anyhow::Result<[u8; 32]> {
    let bytes = serde_json::to_vec(body)?;
    ensure!(bytes.len() <= MAX_HEAD_BYTES, "catalog head payload too large");
    let mut hash = Sha256::new();
    hash.update(b"engine.catalog.head.v3\0");
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
    Ok(hash.finalize().into())
}
