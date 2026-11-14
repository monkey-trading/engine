//! A file sealed to a release alone, not to the vault's generation of it, which the delivered secrets
//! (`tee::secrets`) and the member's certificates (`tee::certificates`) are kept in: one file per
//! release and kind, encrypted with AES-256-GCM under the key the platform derives from the
//! measurement (`Sealer`), with the kind's tag, the measurement, the config hash, and the TCB the key
//! was derived at bound to the ciphertext. Only the same code under the same config on the same
//! platform opens it, and only as the kind it was sealed as.

use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use ring::aead::{Aad, NONCE_LEN, Nonce};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::tee::sealing::{Release, Sealer, aead_key, write_replacing};

/// The sealed file layout this build writes and reads.
const FORMAT: u32 = 1;

/// What a file sealed to a release holds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Kind {
    /// Prefixes the authenticated data, so no other ciphertext under the platform's key opens as this
    /// kind.
    pub tag: &'static [u8],
    /// The file name after the release id.
    pub suffix: &'static str,
    /// What it holds, as its errors name it.
    pub what: &'static str,
}

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

/// This release's file of one kind.
pub(crate) struct ReleaseFile {
    path: PathBuf,
    kind: Kind,
    release: Release,
    /// The platform's committed TCB when the file was opened, which each write seals at.
    committed_tcb: u64,
    sealer: Box<dyn Sealer>,
}

impl ReleaseFile {
    /// This release's file of `kind` in `dir`, this code running the config whose SHA-256 is
    /// `config_sha256`.
    /// Error cases:
    /// - the platform cannot report
    pub fn new(dir: &Path, kind: Kind, sealer: Box<dyn Sealer>, config_sha256: [u8; 32]) -> anyhow::Result<Self> {
        let platform = sealer.platform()?;
        let release = Release { launch_digest: platform.launch_digest, config_sha256 };
        Ok(Self { path: file_for(dir, &release, &kind), kind, release, committed_tcb: platform.tcb, sealer })
    }

    /// The release the file is sealed to.
    pub fn release(&self) -> Release {
        self.release
    }

    /// The plaintext sealed in the file, or `None` when it is not there.
    /// Error cases:
    /// - the file cannot be read, is not a sealed file this build reads, names another release, or
    ///   does not decrypt under this release's key as this kind
    pub fn read(&self) -> anyhow::Result<Option<Vec<u8>>> {
        let (path, what) = (&self.path, self.kind.what);
        let text = match std::fs::read(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let sealed: SealedFile = serde_json::from_slice(&text)
            .map_err(|_| anyhow::anyhow!("{} is not a sealed {what} file", path.display()))?;
        ensure!(sealed.format == FORMAT, "{} has format {}, not {FORMAT}", path.display(), sealed.format);
        ensure!(
            sealed.launch_digest == hex::encode(self.release.launch_digest)
                && sealed.config_sha256 == hex::encode(self.release.config_sha256),
            "{} was sealed by another release",
            path.display()
        );
        let key = aead_key(self.sealer.as_ref(), sealed.tcb)?;
        let nonce: [u8; NONCE_LEN] = hex::decode(&sealed.nonce)
            .ok()
            .and_then(|nonce| nonce.try_into().ok())
            .with_context(|| format!("{} has a malformed nonce", path.display()))?;
        let mut ciphertext = hex::decode(&sealed.ciphertext).context("the sealed ciphertext is not hex")?;
        let plaintext =
            key.open_in_place(Nonce::assume_unique_for_key(nonce), self.aad(sealed.tcb), &mut ciphertext)
                .map_err(|_| anyhow::anyhow!("{} does not open under this release's sealing key", path.display()))?;
        Ok(Some(plaintext.to_vec()))
    }

    /// Seals `plaintext` at the committed TCB into a new file beside the current one, syncs it, and
    /// renames it over, so a crash leaves one whole file or the other.
    /// Error cases:
    /// - the platform refuses the key, or encryption, the write, a sync, or the rename fails
    pub fn write(&self, mut plaintext: Vec<u8>) -> anyhow::Result<()> {
        let key = aead_key(self.sealer.as_ref(), self.committed_tcb)?;
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| anyhow::anyhow!("the RNG failed"))?;
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), self.aad(self.committed_tcb), &mut plaintext)
            .map_err(|_| anyhow::anyhow!("sealing the {} failed", self.kind.what))?;
        let sealed = SealedFile {
            format: FORMAT,
            launch_digest: hex::encode(self.release.launch_digest),
            config_sha256: hex::encode(self.release.config_sha256),
            tcb: self.committed_tcb,
            nonce: hex::encode(nonce),
            ciphertext: hex::encode(plaintext),
        };
        write_replacing(&self.path, &serde_json::to_vec(&sealed)?)
    }

    /// Binds the kind, the release, its config included, and the TCB the key was derived at to the
    /// ciphertext.
    fn aad(&self, tcb: u64) -> Aad<Vec<u8>> {
        let release = &self.release;
        Aad::from([self.kind.tag, &release.launch_digest, &release.config_sha256, &tcb.to_be_bytes()].concat())
    }
}

/// The file `release`'s file of `kind` is sealed in.
pub(crate) fn file_for(dir: &Path, release: &Release, kind: &Kind) -> PathBuf {
    dir.join(format!("{}.{}", hex::encode(release.id()), kind.suffix))
}
