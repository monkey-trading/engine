//! A rotation-set member's standby key (#595): a secp256k1 signing key, and the TLS key it would serve
//! with, which the member's enclave generates for its current release and seals under that release
//! apart from the turn's keys, in a file of its own kind that never opens as them
//! (`tee::release_file`). Another enclave vouches for it once its attestation verifies (`vouch`), and
//! the candidate registers it with the schedule, which promotes it to the vault's signer, in a new
//! signer epoch, once the operator in effect misses its heartbeats.
//!
//! The seal opens only while the schedule, read at the latest block a strict majority of the custody
//! providers agrees on (`chain`), lists the key as the candidate's promotable standby key, or as the
//! signer a promotion installed that is still its turn's latest assignment, under the signer epoch it
//! started and the generation it reserved, until the key is resealed as the turn's keys under that
//! generation. In every other state the key holds no authority, and a start seals a new one in its
//! place, which the candidate registers. On promotion the node keeps the key in memory and reseals it
//! as the turn's keys (`KeyStore`) under the promotion's generation, so a restart opens it as those;
//! the standby seal is deleted only once that file is durable.
//!
//! The host holds the file and bounds what each thing it can do with it achieves:
//! - deleting it, or restarting the enclave before its registration lands, gives a new key, which the
//!   candidate registers in place of the old one;
//! - restoring a copy after the key's turn or promotion has moved on gives a key the schedule no
//!   longer lists, which does not open; restoring one beside a deleted turn-key file, within the same
//!   promotion, opens a copy of the signer's key the host kept, the two-copies residual `SECURITY.md`
//!   states;
//! - altering it, or giving another release's or kind's file its name, fails authenticated
//!   decryption, and the start refuses.

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, ensure};
use engine_core::eip712::Signature;
use engine_core::types::AccountId;
use rcgen::{KeyPair, PublicKeyData as _};
use sha2::{Digest as _, Sha256};
use tracing::info;

use crate::tee::EnclaveKey;
use crate::tee::keys::KeySet;
use crate::tee::release_file::{Kind, ReleaseFile, file_for};
use crate::tee::sealing::{KeyStore, Platform, Release, Sealer};

pub(crate) mod chain;
use chain::{Promotion, StandbyChain, StandbyDomain, StandbyState, standby_state};
pub(crate) mod vouch;

/// The standby seal, whose tag opens neither the turn's keys nor another kind as standby keys.
const STANDBY: Kind = Kind { tag: b"engine.sealed-standby.v1", suffix: "standby.json", what: "standby keys" };

/// What a start finds of its standby keys.
#[derive(Debug)]
pub(crate) enum StandbyStart {
    /// The keys to vouch for and register: the sealed ones while the schedule lists them as
    /// promotable, or new ones sealed now (`generated`).
    Standby { keys: StandbyKeys, generated: bool },
    /// The sealed key is the vault's signer by a promotion that is still its turn's latest
    /// assignment, and its reseal under the promotion's generation is not yet durable: the caller
    /// rechecks the promotion's prefixes against its log and reseals (`StandbyKeys::reseal`) before
    /// its first command.
    Promoted { keys: StandbyKeys, promotion: Promotion },
}

/// This release's standby keys, opened or generated, and their seal.
pub(crate) struct StandbyKeys {
    file: ReleaseFile,
    /// The sealing directory, which holds the turn's keys the reseal writes.
    dir: PathBuf,
    sealer: Arc<dyn Sealer>,
    platform: Platform,
    /// The signing key and the TLS key, as the reseal seals them as the turn's keys.
    keys: KeySet,
    signing: EnclaveKey,
    tls_public_key_sha256: [u8; 32],
}

/// The platform's sealing, shared by the standby seal and the turn's keys the reseal writes.
struct Shared(Arc<dyn Sealer>);

impl Sealer for Shared {
    /// The shared sealer's report.
    /// Error cases:
    /// - the platform cannot report
    fn platform(&self) -> anyhow::Result<Platform> {
        self.0.platform()
    }

    /// The shared sealer's key at `tcb`.
    /// Error cases:
    /// - the platform refuses the request
    fn sealing_key(&self, tcb: u64) -> anyhow::Result<[u8; 32]> {
        self.0.sealing_key(tcb)
    }
}

impl fmt::Debug for StandbyKeys {
    /// The key's address, without its secrets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StandbyKeys").field("address", &self.address()).finish_non_exhaustive()
    }
}

impl StandbyKeys {
    /// This release's standby keys in `dir`, for this code running the config whose SHA-256 is
    /// `config_sha256`, as `candidate`'s: the sealed ones when the schedule in `domain` lists them as
    /// promotable, or as a promotion's signer whose reseal is not yet durable; otherwise new keys,
    /// sealed over the old. A promotion's key already resealed as the turn's keys is not opened again.
    /// Error cases:
    /// - the platform cannot report or derive the key, or the seal cannot be read, written and synced
    /// - the sealed file does not open under this release as standby keys
    /// - the chain cannot be read, which leaves the seal as it was
    /// - a turn-key file for the promotion's generation holds another key
    pub(crate) async fn open_or_generate(
        dir: &Path,
        sealer: Box<dyn Sealer>,
        config_sha256: [u8; 32],
        chain: &impl StandbyChain,
        domain: &StandbyDomain,
        candidate: AccountId,
    ) -> anyhow::Result<StandbyStart> {
        let sealer: Arc<dyn Sealer> = Arc::from(sealer);
        let platform = sealer.platform()?;
        let file = ReleaseFile::new(dir, STANDBY, Box::new(Shared(sealer.clone())), config_sha256)?;
        let Some(plaintext) = file.read()? else {
            let keys = Self::new(file, dir.to_owned(), sealer, platform, KeySet::generate()?)?;
            keys.seal()?;
            info!(key = %keys.address(), "sealed a new standby key");
            return Ok(StandbyStart::Standby { keys, generated: true });
        };
        let held = serde_json::from_slice(&plaintext).context("the sealed standby keys do not parse")?;
        let keys = Self::new(file, dir.to_owned(), sealer, platform, held)?;
        let reason = match standby_state(chain, domain, candidate, keys.address()).await? {
            StandbyState::Promotable => return Ok(StandbyStart::Standby { keys, generated: false }),
            StandbyState::Promoted(promotion) => match keys.resealed(promotion.generation)? {
                None => return Ok(StandbyStart::Promoted { keys, promotion }),
                Some(_) => "it is already resealed as the turn's keys under the promotion's generation",
            },
            StandbyState::Unusable(reason) => reason,
        };
        info!(key = %keys.address(), reason, "the sealed standby key holds no authority: sealing a new one");
        let keys = Self::new(keys.file, keys.dir, keys.sealer, platform, KeySet::generate()?)?;
        keys.seal()?;
        info!(key = %keys.address(), "sealed a new standby key");
        Ok(StandbyStart::Standby { keys, generated: true })
    }

    /// The signing key's address.
    pub(crate) fn address(&self) -> AccountId {
        AccountId(self.signing.address())
    }

    /// Signs `digest` as it is, with no message prefix: 65-byte `r || s || v`, `v` in {27, 28}.
    pub(crate) fn sign(&self, digest: [u8; 32]) -> Signature {
        self.signing.sign_digest(digest).parse().expect("sign_digest writes 65 bytes of 0x hex")
    }

    /// The TLS key the member would serve with once promoted.
    /// Error cases:
    /// - the sealed key does not load
    pub(crate) fn tls_key(&self) -> anyhow::Result<KeyPair> {
        self.keys.tls_key()
    }

    /// The SHA-256 of the TLS key's DER SubjectPublicKeyInfo, which the attestation binds.
    pub(crate) fn tls_public_key_sha256(&self) -> [u8; 32] {
        self.tls_public_key_sha256
    }

    /// The release the keys are sealed for.
    pub(crate) fn release(&self) -> Release {
        self.file.release()
    }

    /// What the platform reported when the keys were opened.
    pub(crate) fn platform(&self) -> Platform {
        self.platform
    }

    /// Reseals the keys as the turn's keys under the promotion's `generation`, in the file a restart
    /// opens as this release's keys in that generation, then deletes the standby seal, only once
    /// that file is durable. A file already there must hold this key: one an interrupted reseal wrote.
    /// Error cases:
    /// - the turn's file cannot be opened, or holds another key, which keeps the standby seal
    /// - the turn's file cannot be written and synced, which keeps the standby seal
    /// - the standby seal cannot be deleted, or the directory synced
    pub(crate) fn reseal(self, generation: u64) -> anyhow::Result<KeyStore> {
        let store = match self.resealed(generation)? {
            Some(store) => store,
            None => KeyStore::create(
                &self.dir,
                self.sealer.as_ref(),
                self.release().config_sha256,
                generation,
                self.keys.clone(),
            )?,
        };
        let path = file_for(&self.dir, &self.release(), &STANDBY);
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e).with_context(|| format!("deleting {}", path.display()));
        }
        File::open(&self.dir)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("syncing {}", self.dir.display()))?;
        info!(key = %self.address(), generation, "resealed the standby key as the turn's keys");
        Ok(store)
    }

    /// The turn's keys sealed for this release in `generation`, when that file holds this key, or
    /// `None` when there is no file.
    /// Error cases:
    /// - the file does not open, or holds another key
    fn resealed(&self, generation: u64) -> anyhow::Result<Option<KeyStore>> {
        let config_sha256 = self.release().config_sha256;
        let Some(store) = KeyStore::open(&self.dir, self.sealer.as_ref(), config_sha256, generation)? else {
            return Ok(None);
        };
        let sealed = AccountId(store.keys().enclave_key()?.address());
        ensure!(
            sealed == self.address(),
            "the turn's keys for generation {generation} hold {sealed}, not the standby key {}",
            self.address()
        );
        Ok(Some(store))
    }

    /// Standby keys holding `keys`, sealed in `file`.
    /// Error cases:
    /// - the signing key or the TLS key does not load
    fn new(
        file: ReleaseFile,
        dir: PathBuf,
        sealer: Arc<dyn Sealer>,
        platform: Platform,
        keys: KeySet,
    ) -> anyhow::Result<Self> {
        let signing = keys.enclave_key()?;
        let tls_public_key_sha256 = Sha256::digest(keys.tls_key()?.subject_public_key_info()).into();
        Ok(Self { file, dir, sealer, platform, keys, signing, tls_public_key_sha256 })
    }

    /// Seals the keys over the standby seal.
    /// Error cases:
    /// - encryption, the write, a sync, or the rename fails
    fn seal(&self) -> anyhow::Result<()> {
        self.file.write(serde_json::to_vec(&self.keys).context("encoding the standby keys")?)
    }
}

#[cfg(test)]
impl StandbyKeys {
    /// New standby keys sealed in `dir` under the mock sealer, for tests outside this module.
    pub(crate) fn for_test(dir: &Path) -> Self {
        let sealer: Arc<dyn Sealer> = Arc::new(crate::tee::sealing::MockSealer { launch_digest: [0x11; 48] });
        let platform = sealer.platform().unwrap();
        let file = ReleaseFile::new(dir, STANDBY, Box::new(Shared(sealer.clone())), [0xc1; 32]).unwrap();
        let keys = Self::new(file, dir.to_owned(), sealer, platform, KeySet::generate().unwrap()).unwrap();
        keys.seal().unwrap();
        keys
    }
}

#[cfg(test)]
mod tests;
