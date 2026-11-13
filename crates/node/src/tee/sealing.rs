//! The enclave's keys on the host's data disk, sealed to this release: its launch measurement and the
//! config file it runs with, together `Release`, which the vault's governance names, in the vault's
//! generation of it. Each release keeps one file per generation, named for its release id and the
//! generation and encrypted with AES-256-GCM under a key the platform derives from the measurement
//! (`Sealer`), with the config hash and the generation bound to the ciphertext, so only the same
//! code under the same config on the same platform, in the same generation, opens it. A restart
//! opens it; another release, a new config included, cannot, nor can this release once governance
//! has named it again, and each takes the keys from the running release instead (`handover`).
//!
//! The host holds the file and bounds what each thing it can do with it achieves:
//! - deleting it leaves the release without keys, so the node refuses to start until the operator
//!   asks for new ones, which the vault does not name until governance registers them;
//! - restoring an older copy gives keys this release sealed in this generation: the same ones, or
//!   ones the vault has since retired, which the startup signer check refuses; an older copy
//!   without the account or certificate only orders them again;
//! - altering it, or giving another release's or generation's file its name, fails authenticated
//!   decryption, and the node refuses to start.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{Context, ensure};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sha3::{Digest as _, Keccak256};

use crate::tee::keys::KeySet;

/// The sealed file layout this build writes and reads.
const FORMAT: u32 = 1;
/// Prefixes the authenticated data, so no other ciphertext under the platform's key opens as keys.
const AAD_TAG: &[u8] = b"engine.sealed-keys.v1";

/// What the platform reports about the running release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Platform {
    /// The launch measurement.
    pub launch_digest: [u8; 48],
    /// The guest policy the launch ran under.
    pub policy: u64,
    /// The TCB the platform has committed to, packed as its derived-key request takes it.
    pub tcb: u64,
}

/// A release: the code the platform measured and the config it runs with, which governance names
/// together on the vault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Release {
    pub launch_digest: [u8; 48],
    pub config_sha256: [u8; 32],
}

/// The platform's sealing: its report on this release, and the key it derives for this release.
pub(crate) trait Sealer: Send + Sync {
    /// This release's launch measurement and policy, and the platform's committed TCB.
    /// Error cases:
    /// - the platform cannot report
    fn platform(&self) -> anyhow::Result<Platform>;

    /// The 32-byte key the platform derives for this release's measurement and policy at `tcb`,
    /// which must not exceed the committed TCB.
    /// Error cases:
    /// - the platform refuses the request
    fn sealing_key(&self, tcb: u64) -> anyhow::Result<[u8; 32]>;
}

/// A development stand-in for the platform's sealing, and not sealing: its key is a public function
/// of the configured stand-in measurement, so anyone can open its files. Only the mock TEE uses it,
/// and a `production` build refuses that provider.
pub(crate) struct MockSealer {
    pub launch_digest: [u8; 48],
}

/// The sealed file's contents, all public.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedFile {
    format: u32,
    /// The measurement of the release that sealed it, hex.
    launch_digest: String,
    /// The SHA-256 of the config it ran with, hex.
    config_sha256: String,
    /// The vault's generation of the release it was sealed in.
    generation: u64,
    /// The TCB its key was derived at.
    tcb: u64,
    nonce: String,
    ciphertext: String,
}

/// This release's sealed keys: the file, its key, and what it holds. After startup only the
/// certificate task and, when a daily turn names this release in another generation, the sequencer
/// write, so the lock is rarely contended; it keeps the file, its generation and the keys in step.
pub(crate) struct KeyStore {
    platform: Platform,
    config_sha256: [u8; 32],
    /// The TCB the file's key is derived at, kept across reseals.
    tcb: u64,
    key: LessSafeKey,
    sealed: Mutex<Sealed>,
}

/// The file the keys are sealed in, the vault's generation of the release it is sealed in, and the
/// keys it holds.
struct Sealed {
    path: PathBuf,
    generation: u64,
    keys: KeySet,
}

impl Sealer for MockSealer {
    /// The configured stand-in measurement, with no policy and TCB.
    /// Error cases:
    /// - none
    fn platform(&self) -> anyhow::Result<Platform> {
        Ok(Platform { launch_digest: self.launch_digest, policy: 0, tcb: 0 })
    }

    /// A public hash of the stand-in measurement and `tcb`.
    /// Error cases:
    /// - none
    fn sealing_key(&self, tcb: u64) -> anyhow::Result<[u8; 32]> {
        let mut hash = Sha256::new();
        hash.update(b"engine.mock-sealing.v1: public, development only");
        hash.update(self.launch_digest);
        hash.update(tcb.to_be_bytes());
        Ok(hash.finalize().into())
    }
}

impl Release {
    /// The id the vault records: `keccak256` of the measurement and the config hash.
    pub fn id(&self) -> [u8; 32] {
        Keccak256::digest([self.launch_digest.as_slice(), &self.config_sha256].concat()).into()
    }
}

impl KeyStore {
    /// The keys sealed in `dir` for this release, this code running the config whose SHA-256 is
    /// `config_sha256`, in the vault's `generation` of it, or `None` when it has sealed none there.
    /// Error cases:
    /// - the platform cannot report or derive the key
    /// - the file cannot be read, is not a sealed file this build reads, names another release or
    ///   generation, or does not decrypt to a key set
    pub fn open(
        dir: &Path,
        sealer: &dyn Sealer,
        config_sha256: [u8; 32],
        generation: u64,
    ) -> anyhow::Result<Option<Self>> {
        let platform = sealer.platform()?;
        let release = Release { launch_digest: platform.launch_digest, config_sha256 };
        let path = file_for(dir, &release, generation);
        let text = match std::fs::read(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let sealed: SealedFile =
            serde_json::from_slice(&text).with_context(|| format!("{} is not a sealed key file", path.display()))?;
        ensure!(sealed.format == FORMAT, "{} has format {}, not {FORMAT}", path.display(), sealed.format);
        ensure!(
            sealed.launch_digest == hex::encode(platform.launch_digest)
                && sealed.config_sha256 == hex::encode(config_sha256)
                && sealed.generation == generation,
            "{} was sealed by another release or generation",
            path.display()
        );
        let key = aead_key(sealer, sealed.tcb)?;
        let nonce: [u8; NONCE_LEN] = hex::decode(&sealed.nonce)
            .ok()
            .and_then(|nonce| nonce.try_into().ok())
            .with_context(|| format!("{} has a malformed nonce", path.display()))?;
        let mut ciphertext = hex::decode(&sealed.ciphertext).context("the sealed ciphertext is not hex")?;
        let plaintext = key
            .open_in_place(Nonce::assume_unique_for_key(nonce), aad(&release, generation, sealed.tcb), &mut ciphertext)
            .map_err(|_| anyhow::anyhow!("{} does not open under this release's sealing key", path.display()))?;
        let keys: KeySet = serde_json::from_slice(plaintext).context("the sealed keys do not parse")?;
        let tcb = sealed.tcb;
        Ok(Some(Self { platform, config_sha256, tcb, key, sealed: Mutex::new(Sealed { path, generation, keys }) }))
    }

    /// Seals `keys` as this release's first file in `dir` for the vault's `generation` of it, for
    /// this code running the config whose SHA-256 is `config_sha256`, at the platform's committed TCB.
    /// Error cases:
    /// - the platform cannot report or derive the key
    /// - the release already has a file there for that generation, or it cannot be written and synced
    pub fn create(
        dir: &Path,
        sealer: &dyn Sealer,
        config_sha256: [u8; 32],
        generation: u64,
        keys: KeySet,
    ) -> anyhow::Result<Self> {
        let platform = sealer.platform()?;
        let path = file_for(dir, &Release { launch_digest: platform.launch_digest, config_sha256 }, generation);
        ensure!(!path.exists(), "{} already exists", path.display());
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let key = aead_key(sealer, platform.tcb)?;
        let sealed = Mutex::new(Sealed { path, generation, keys });
        let store = Self { platform, config_sha256, tcb: platform.tcb, key, sealed };
        let sealed = store.lock();
        store.write(&sealed.path, generation, &sealed.keys)?;
        drop(sealed);
        Ok(store)
    }

    /// A copy of the keys.
    pub fn keys(&self) -> KeySet {
        self.lock().keys.clone()
    }

    /// What the platform reported when the store opened.
    pub fn platform(&self) -> Platform {
        self.platform
    }

    /// The release the keys are sealed for.
    pub fn release(&self) -> Release {
        Release { launch_digest: self.platform.launch_digest, config_sha256: self.config_sha256 }
    }

    /// The vault's generation of the release the keys are sealed in.
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Applies `change` and seals the result over the file; on failure the file and the keys keep
    /// what they held.
    /// Error cases:
    /// - the file cannot be written and synced
    pub fn update(&self, change: impl FnOnce(&mut KeySet)) -> anyhow::Result<()> {
        let mut sealed = self.lock();
        let mut changed = sealed.keys.clone();
        change(&mut changed);
        self.write(&sealed.path, sealed.generation, &changed)?;
        sealed.keys = changed;
        Ok(())
    }

    /// Seals the keys again as this release's file for the vault's `generation` of it, beside the
    /// current one, and seals every later change there: a daily turn that names this release in a
    /// new generation while it holds the keys. The earlier file stays, and no start opens it again,
    /// since no later turn or release names its generation. On failure the store keeps its file.
    /// Error cases:
    /// - the file cannot be written and synced
    pub fn reseal(&self, generation: u64) -> anyhow::Result<()> {
        let mut sealed = self.lock();
        if sealed.generation == generation {
            return Ok(());
        }
        let dir = sealed.path.parent().context("the sealed file has a directory")?;
        let path = file_for(dir, &self.release(), generation);
        self.write(&path, generation, &sealed.keys)?;
        (sealed.path, sealed.generation) = (path, generation);
        Ok(())
    }

    /// Seals `keys` for `generation` into a new file beside `path`, syncs it, and renames it over
    /// `path`, so a crash leaves one whole file or the other.
    /// Error cases:
    /// - encryption, the write, a sync, or the rename fails
    fn write(&self, path: &Path, generation: u64, keys: &KeySet) -> anyhow::Result<()> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| anyhow::anyhow!("the RNG failed"))?;
        let mut ciphertext = serde_json::to_vec(keys).context("encoding the keys")?;
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                aad(&self.release(), generation, self.tcb),
                &mut ciphertext,
            )
            .map_err(|_| anyhow::anyhow!("sealing the keys failed"))?;
        let sealed = SealedFile {
            format: FORMAT,
            launch_digest: hex::encode(self.platform.launch_digest),
            config_sha256: hex::encode(self.config_sha256),
            generation,
            tcb: self.tcb,
            nonce: hex::encode(nonce),
            ciphertext: hex::encode(ciphertext),
        };
        write_replacing(path, &serde_json::to_vec(&sealed)?)
    }

    /// The sealed file and its keys, whether or not a panic interrupted an earlier holder: `update`
    /// and `reseal` replace them whole.
    fn lock(&self) -> MutexGuard<'_, Sealed> {
        self.sealed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The file `release`'s keys are sealed in for the vault's `generation` of it, named for both.
pub(crate) fn file_for(dir: &Path, release: &Release, generation: u64) -> PathBuf {
    dir.join(format!("{}-{generation}.sealed.json", hex::encode(release.id())))
}

/// Writes `bytes` to a new file beside `path`, syncs it, and renames it over `path`, so a crash
/// leaves one whole file or the other.
/// Error cases:
/// - the write, a sync, or the rename fails
pub(crate) fn write_replacing(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().context("the sealed file has a directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let partial = path.with_extension("partial");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&partial)
        .with_context(|| format!("creating {}", partial.display()))?;
    file.write_all(bytes).and_then(|()| file.sync_all()).context("writing the sealed file")?;
    std::fs::rename(&partial, path).with_context(|| format!("replacing {}", path.display()))?;
    File::open(dir).and_then(|dir| dir.sync_all()).with_context(|| format!("syncing {}", dir.display()))
}

/// The AEAD key the platform derives at `tcb`.
/// Error cases:
/// - the platform refuses the derivation
pub(crate) fn aead_key(sealer: &dyn Sealer, tcb: u64) -> anyhow::Result<LessSafeKey> {
    let derived = sealer.sealing_key(tcb).context("deriving the sealing key")?;
    Ok(LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &derived).expect("a 32-byte key is an AES-256 key")))
}

/// Binds the release, its config included, its generation, and the TCB the key was derived at to the
/// ciphertext.
fn aad(release: &Release, generation: u64, tcb: u64) -> Aad<Vec<u8>> {
    let (generation, tcb) = (generation.to_be_bytes(), tcb.to_be_bytes());
    Aad::from([AAD_TAG, &release.launch_digest, &release.config_sha256, &generation, &tcb].concat())
}

#[cfg(test)]
mod tests;
