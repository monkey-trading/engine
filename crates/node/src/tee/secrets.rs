//! The delivered secrets on the host's data disk, sealed to this release: one file per release,
//! encrypted with AES-256-GCM under the key the platform derives from the measurement (`Sealer`),
//! with the config hash bound to the ciphertext, so only the same code under the same config on the
//! same platform opens it. A restart opens it; another release, a new config included, cannot, and
//! takes the secrets from the running release or from the operator instead.
//!
//! Unlike the keys (`sealing`), the file is not bound to the vault's generation. The generation
//! fences a stale signer, and these sign nothing. They must also open before the vault can be read,
//! since reading it can take a delivered key, and a release governance names again is still its
//! operator's release, so they stay valid for it.
//!
//! The host holds the file and bounds what each thing it can do with it achieves:
//! - deleting it, or altering it so it no longer opens, disables what it held, under an alert, until
//!   the operator delivers again, which replaces it;
//! - restoring an older copy gives the older bundle this release held. Nothing on this disk can tell
//!   it from the newest, and nothing else records the version. What that buys is bounded: every URL's
//!   origin is fixed by the attested config, so an older copy can only hold an older key for the same
//!   provider, which the operator revoked if it rotated for cause, or an older webhook, whose pages
//!   the host could drop anyway. The version shows in the attested status (`engine admin
//!   secrets-status`), and delivering a higher one repairs it;
//! - giving another release's file this one's name fails authenticated decryption.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{Context, ensure};
use ring::aead::{Aad, LessSafeKey, NONCE_LEN, Nonce};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::secrets::SignedBundle;
use crate::tee::sealing::{Release, Sealer, aead_key, write_replacing};

/// The sealed file layout this build writes and reads.
const FORMAT: u32 = 1;
/// Prefixes the authenticated data, so no other ciphertext under the platform's key opens as secrets
/// and these do not open as keys.
const AAD_TAG: &[u8] = b"engine.sealed-secrets.v1";

/// The sealed file's contents, all public but the ciphertext's.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedFile {
    format: u32,
    /// The measurement of the release that sealed it, hex.
    launch_digest: String,
    /// The SHA-256 of the config it ran with, hex.
    config_sha256: String,
    /// The TCB its key was derived at.
    tcb: u64,
    nonce: String,
    ciphertext: String,
}

/// This release's sealed secrets, and whether its file opened. The delivery listener alone writes
/// after startup, so the lock is never contended; a handover to the next release reads through it.
pub(crate) struct SecretStore {
    path: PathBuf,
    release: Release,
    /// The platform's committed TCB when the store opened, which each write seals at.
    committed_tcb: u64,
    /// Why the file on disk did not open, until a delivery replaces it.
    unreadable: Mutex<Option<String>>,
    bundle: Mutex<Option<SignedBundle>>,
    sealer: Box<dyn Sealer>,
}

/// What `replace` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Replaced {
    /// The bundle is sealed as this release's newest.
    Sealed,
    /// The same bundle was already sealed: a retry after a lost receipt.
    AlreadySealed,
}

impl SecretStore {
    /// The store for this release in `dir`, this code running the config whose SHA-256 is
    /// `config_sha256`. A file that does not open leaves it empty, with `unreadable` saying why:
    /// the node then runs without what it held, and a delivery replaces it.
    /// Error cases:
    /// - the platform cannot report
    pub fn open(dir: &Path, sealer: Box<dyn Sealer>, config_sha256: [u8; 32]) -> anyhow::Result<Self> {
        let platform = sealer.platform()?;
        let release = Release { launch_digest: platform.launch_digest, config_sha256 };
        let path = file_for(dir, &release);
        let (bundle, unreadable) = match load(&path, sealer.as_ref(), &release) {
            Ok(bundle) => (bundle, None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        Ok(Self {
            path,
            release,
            committed_tcb: platform.tcb,
            unreadable: Mutex::new(unreadable),
            bundle: Mutex::new(bundle),
            sealer,
        })
    }

    /// The release the secrets are sealed for.
    pub fn release(&self) -> Release {
        self.release
    }

    /// Why the file on disk did not open, if it did not.
    pub fn unreadable(&self) -> Option<String> {
        self.unreadable.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The sealed bundle, if there is one.
    pub fn current(&self) -> Option<SignedBundle> {
        self.lock().clone()
    }

    /// The sealed bundle, telling `running` its version under the same lock `replace` takes, so a
    /// bundle sealed from here on is one this start does not run, and no delivery falls between.
    pub fn current_running(&self, running: impl FnOnce(Option<u64>)) -> Option<SignedBundle> {
        let held = self.lock();
        running(held.as_ref().map(|signed| signed.bundle.version));
        held.clone()
    }

    /// The sealed bundle's version, if there is one.
    pub fn version(&self) -> Option<u64> {
        self.lock().as_ref().map(|signed| signed.bundle.version)
    }

    /// Seals `signed` over the file unless the store holds a newer one. The same version with the
    /// same secrets is a retry after a lost receipt, and changes nothing unless a newer operator key
    /// signed it: that signature is the one an upgrade to a config naming that key will check.
    /// Error cases:
    /// - the store holds this version with other secrets, or a higher one
    /// - the file cannot be written and synced, which leaves the file and the store as they were
    pub fn replace(&self, signed: SignedBundle) -> anyhow::Result<Replaced> {
        let mut held = self.lock();
        let mut replaced = Replaced::Sealed;
        if let Some(held) = held.as_ref() {
            let (version, newest) = (signed.bundle.version, held.bundle.version);
            ensure!(
                version != newest || signed.bundle == held.bundle,
                "version {version} is already sealed with other secrets: a rotation takes a higher version"
            );
            if signed == *held {
                return Ok(Replaced::AlreadySealed);
            }
            if version == newest {
                replaced = Replaced::AlreadySealed;
            } else {
                ensure!(version > newest, "version {version} is older than the sealed version {newest}");
            }
        }
        self.write(&signed)?;
        *held = Some(signed);
        *self.unreadable.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(replaced)
    }

    /// Seals `signed` into a new file beside the current one and renames it over.
    /// Error cases:
    /// - the platform refuses the key, or encryption, the write, a sync, or the rename fails
    fn write(&self, signed: &SignedBundle) -> anyhow::Result<()> {
        let key = aead_key(self.sealer.as_ref(), self.committed_tcb)?;
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| anyhow::anyhow!("the RNG failed"))?;
        let mut ciphertext = signed.to_bytes().context("encoding the bundle")?;
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            aad(&self.release, self.committed_tcb),
            &mut ciphertext,
        )
        .map_err(|_| anyhow::anyhow!("sealing the secrets failed"))?;
        let sealed = SealedFile {
            format: FORMAT,
            launch_digest: hex::encode(self.release.launch_digest),
            config_sha256: hex::encode(self.release.config_sha256),
            tcb: self.committed_tcb,
            nonce: hex::encode(nonce),
            ciphertext: hex::encode(ciphertext),
        };
        write_replacing(&self.path, &serde_json::to_vec(&sealed)?)
    }

    /// The bundle, whether or not a panic interrupted an earlier holder: `replace` swaps it whole.
    fn lock(&self) -> MutexGuard<'_, Option<SignedBundle>> {
        self.bundle.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The bundle `path` holds, or `None` when the file is not there.
/// Error cases:
/// - the file cannot be read, is not a sealed file this build reads, names another release, or does
///   not decrypt to a bundle
fn load(path: &Path, sealer: &dyn Sealer, release: &Release) -> anyhow::Result<Option<SignedBundle>> {
    let text = match std::fs::read(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let sealed: SealedFile = serde_json::from_slice(&text)
        .map_err(|_| anyhow::anyhow!("{} is not a sealed secrets file", path.display()))?;
    ensure!(sealed.format == FORMAT, "{} has format {}, not {FORMAT}", path.display(), sealed.format);
    ensure!(
        sealed.launch_digest == hex::encode(release.launch_digest)
            && sealed.config_sha256 == hex::encode(release.config_sha256),
        "{} was sealed by another release",
        path.display()
    );
    let key: LessSafeKey = aead_key(sealer, sealed.tcb)?;
    let nonce: [u8; NONCE_LEN] = hex::decode(&sealed.nonce)
        .ok()
        .and_then(|nonce| nonce.try_into().ok())
        .with_context(|| format!("{} has a malformed nonce", path.display()))?;
    let mut ciphertext = hex::decode(&sealed.ciphertext).context("the sealed ciphertext is not hex")?;
    let plaintext =
        key.open_in_place(Nonce::assume_unique_for_key(nonce), aad(release, sealed.tcb), &mut ciphertext)
            .map_err(|_| anyhow::anyhow!("{} does not open under this release's sealing key", path.display()))?;
    SignedBundle::from_bytes(plaintext).map(Some)
}

/// The file `release`'s secrets are sealed in.
pub(crate) fn file_for(dir: &Path, release: &Release) -> PathBuf {
    dir.join(format!("{}.secrets.json", hex::encode(release.id())))
}

/// Binds the release, its config included, and the TCB the key was derived at to the ciphertext.
fn aad(release: &Release, tcb: u64) -> Aad<Vec<u8>> {
    Aad::from([AAD_TAG, &release.launch_digest, &release.config_sha256, &tcb.to_be_bytes()].concat())
}

#[cfg(test)]
mod tests;
