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

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{Context, ensure};

use crate::secrets::SignedBundle;
use crate::tee::release_file::{Kind, ReleaseFile};
use crate::tee::sealing::{Release, Sealer};

/// The secrets' file, whose tag opens neither the keys nor another kind as secrets.
const SECRETS: Kind = Kind { tag: b"engine.sealed-secrets.v1", suffix: "secrets.json", what: "secrets" };

/// This release's sealed secrets, and whether its file opened. The lock serializes the listener's
/// connections, and a start's choice of what it runs against them; a handover to the next release
/// reads through it.
pub(crate) struct SecretStore {
    file: ReleaseFile,
    /// Why the file on disk did not open, until a delivery replaces it.
    unreadable: Mutex<Option<String>>,
    bundle: Mutex<Option<SignedBundle>>,
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
        let file = ReleaseFile::new(dir, SECRETS, sealer, config_sha256)?;
        let (bundle, unreadable) = match load(&file) {
            Ok(bundle) => (bundle, None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        Ok(Self { file, unreadable: Mutex::new(unreadable), bundle: Mutex::new(bundle) })
    }

    /// The release the secrets are sealed for.
    pub fn release(&self) -> Release {
        self.file.release()
    }

    /// Why the file on disk did not open, if it did not.
    pub fn unreadable(&self) -> Option<String> {
        self.unreadable.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The sealed bundle, if there is one.
    pub fn current(&self) -> Option<SignedBundle> {
        self.lock().clone()
    }

    /// The sealed bundle when `complete` finds it whole, else `None`, with the check and the read
    /// under the one lock `replace` takes, so no delivery can replace a complete bundle with a
    /// partial one between a start's completeness check and its choice of what it runs. `running`
    /// is told the version under that same lock, once `complete` has passed, so a bundle sealed
    /// from here on is one this start does not run, and no delivery falls between.
    pub fn current_if_complete(
        &self,
        complete: impl FnOnce(Option<&SignedBundle>) -> bool,
        running: impl FnOnce(Option<u64>),
    ) -> Option<Option<SignedBundle>> {
        let held = self.lock();
        if !complete(held.as_ref()) {
            return None;
        }
        running(held.as_ref().map(|signed| signed.bundle.version));
        Some(held.clone())
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
        self.file.write(signed.to_bytes().context("encoding the bundle")?)
    }

    /// The bundle, whether or not a panic interrupted an earlier holder: `replace` swaps it whole.
    fn lock(&self) -> MutexGuard<'_, Option<SignedBundle>> {
        self.bundle.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The bundle `file` holds, or `None` when the file is not there.
/// Error cases:
/// - the file cannot be read, is not a sealed file this build reads, names another release, or does
///   not decrypt to a bundle
fn load(file: &ReleaseFile) -> anyhow::Result<Option<SignedBundle>> {
    file.read()?.map(|plaintext| SignedBundle::from_bytes(&plaintext)).transpose()
}

/// The file `release`'s secrets are sealed in, for tests.
#[cfg(test)]
pub(crate) fn file_for(dir: &Path, release: &Release) -> std::path::PathBuf {
    crate::tee::release_file::file_for(dir, release, &SECRETS)
}

#[cfg(test)]
mod tests;
