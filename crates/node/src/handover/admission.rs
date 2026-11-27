//! What binds a successor to its own enclave where no registered standby key does. Every enclave that
//! asks for the keys signs its request with its release's standby key (`standby_key`), which only that
//! enclave holds, sealed for that release. The holder takes the key as the successor's when the
//! schedule lists it as the turn's member's promotable standby key, or when an admission names it,
//! signed by the key that answers for the successor: the turn's admin key, the operator the registry
//! names while rotation is on, or the operator key the holder's log records while rotation is off
//! (`predecessor::assignment::ensure_requester`). An admission is EIP-712 typed data under the
//! deployment's domain, which `engine admin sign-admission` writes and the successor reads from
//! `ADMISSION_FILE` in its sealing directory. The host carries the file, and can only withhold it: it
//! names a key no other enclave holds.

use std::path::Path;
use std::sync::Arc;

use alloy::primitives::{Address, B256};
use alloy::sol;
use alloy::sol_types::SolStruct;
use anyhow::Context;
use engine_core::eip712::{self, Signature};
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::log::CheckpointDomain;
use crate::standby_key::StandbyKeys;

/// The admission's file in the successor's sealing directory.
pub(crate) const ADMISSION_FILE: &str = "handover-admission.json";

sol! {
    /// The operator's admission of the enclave that holds `key` as a successor running `release`.
    struct HandoverAdmission {
        address key;
        bytes32 release;
    }
}

/// An admission of the enclave holding `key` as a successor running `release`, and its signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Admission {
    /// The successor enclave's standby key for `release`.
    pub key: AccountId,
    /// The successor's release: the keccak of its launch measurement and config hash.
    pub release: B256,
    /// The signature of the key that answers for the successor, over `digest`.
    pub signature: Signature,
}

/// What binds a successor's request to its own enclave: its release's standby keys, which sign the
/// request, and the admission naming them, if one was given.
pub(crate) struct Requester {
    pub key: Arc<StandbyKeys>,
    pub admission: Option<Admission>,
}

impl Admission {
    /// `key`, of an enclave running `release`, admitted by `signer` for `domain`.
    pub(crate) fn sign(signer: &SigningKey, domain: CheckpointDomain, key: AccountId, release: [u8; 32]) -> Self {
        let release = B256::from(release);
        Self { key, release, signature: eip712::sign(signer, &digest(domain, key, release)) }
    }

    /// The key that signed it for `domain`; `None` for a signature that does not recover.
    pub(crate) fn signer(&self, domain: CheckpointDomain) -> Option<AccountId> {
        eip712::recover(&digest(domain, self.key, self.release), &self.signature)
    }

    /// Whether it names `key` running `release`.
    pub(crate) fn names(&self, key: AccountId, release: [u8; 32]) -> bool {
        self.key == key && self.release == B256::from(release)
    }

    /// The admission in `dir`'s `ADMISSION_FILE`, if there is one.
    /// Error cases:
    /// - the file cannot be read, or does not parse
    pub(crate) fn read(dir: &Path) -> anyhow::Result<Option<Self>> {
        let path = dir.join(ADMISSION_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("{} does not parse", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

impl Requester {
    /// `key`, with the admission in `dir` when it names `key` for `key`'s release; one naming another
    /// key or release, or a file that does not read, is passed over, logged.
    pub(crate) fn with_admission(key: Arc<StandbyKeys>, dir: &Path) -> Self {
        let (address, release) = (key.address(), key.release().id());
        let admission = match Admission::read(dir) {
            Ok(Some(admission)) if admission.names(address, release) => Some(admission),
            Ok(Some(admission)) => {
                warn!(
                    key = %address,
                    "the handover admission in {} names key {} for release {}, not this enclave's: it is not sent",
                    dir.display(),
                    admission.key,
                    admission.release
                );
                None
            }
            Ok(None) => None,
            Err(e) => {
                warn!(key = %address, "the handover admission is not sent: {e:#}");
                None
            }
        };
        info!(
            key = %address,
            release = %hex::encode(release),
            admitted = admission.is_some(),
            "this enclave signs its handover requests with its standby key"
        );
        Self { key, admission }
    }
}

/// The EIP-712 digest an admission of `key` running `release` signs, for `domain`.
fn digest(domain: CheckpointDomain, key: AccountId, release: B256) -> B256 {
    let typed = HandoverAdmission { key: Address::from(key.0), release };
    typed.eip712_signing_hash(&eip712::domain(domain.chain_id, domain.vault))
}

#[cfg(test)]
mod tests;
