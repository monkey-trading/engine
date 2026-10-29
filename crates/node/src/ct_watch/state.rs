//! The watcher's durable state, one JSON file written atomically: which TLS key hashes verified
//! attestations named and when, how far each log has been read, and the certificates still
//! waiting on or raising an alert. The watcher holds no key of the node's, so nothing here is a
//! secret.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// Bounds the attested key hashes kept; the oldest go first. A restart adds one.
pub(super) const MAX_ATTESTED: usize = 4096;
/// Bounds the certificates held for a grace wait or an open alert; a batch that would pass it waits
/// unread until some close.
pub(super) const MAX_FINDINGS: usize = 1024;

#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct State {
    /// The domain this state was kept for: its findings and positions mean nothing for another.
    pub domain: String,
    /// Whether keys here came from a mock attestation (`--allow-mock`). A state file keeps one trust
    /// mode, so keys a mock node named are never trusted by a run that refuses mock attestations.
    pub allow_mock: bool,
    /// Hex SHA-256 of each TLS key a verified attestation named, with when the watcher first saw it.
    pub attested: BTreeMap<String, u64>,
    /// Progress by the hex SHA-256 of a log's public key, so it survives a rename or a new URL.
    pub logs: BTreeMap<String, LogProgress>,
    pub findings: Vec<Finding>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct LogProgress {
    /// The next entry to read; `None` until the log's first tree head fixes the starting point.
    pub next_index: Option<u64>,
    /// When the log was last read up to a signed tree head, in watcher milliseconds.
    pub caught_up_ms: u64,
}

/// A certificate for the domain whose key no verified attestation has named.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Finding {
    pub log: String,
    pub index: u64,
    pub issuer: String,
    /// Hex SHA-256 of the certificate's key.
    pub key_sha256: String,
    /// When the certificate expires, and the finding with it.
    pub not_after_ms: u64,
    /// When the grace wait ends and the finding raises its alert.
    pub decide_at_ms: u64,
    /// When the alert was first raised; `None` while the finding waits out its grace.
    pub alerted_at_ms: Option<u64>,
    /// The alert has ended and its resolve is out; the finding is dropped on the next pass.
    pub resolving: bool,
}

/// The state file, held open under an exclusive lock for as long as the watcher runs.
pub(super) struct StateFile {
    path: PathBuf,
    _lock: File,
}

impl StateFile {
    /// Locks `<path>.lock` and loads the state, or starts empty when the file does not exist.
    /// Error cases:
    /// - another watcher holds the lock, or the file cannot be read or parsed (it is never
    ///   replaced by an empty state, since that would forget the logs' positions)
    pub(super) fn open(path: &Path) -> anyhow::Result<(Self, State)> {
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let lock_path = suffixed(path, ".lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening {}", lock_path.display()))?;
        if lock.try_lock().is_err() {
            bail!("another watcher holds {}", lock_path.display());
        }
        let state = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok((Self { path: path.to_owned(), _lock: lock }, state))
    }

    /// Replaces the file with `state`: written and synced beside it, then renamed over it, so a
    /// crash leaves the old state or the new, never a mix.
    /// Error cases:
    /// - writing, syncing or renaming fails
    pub(super) fn save(&self, state: &State) -> anyhow::Result<()> {
        let temp = suffixed(&self.path, ".tmp");
        let mut file = File::create(&temp).with_context(|| format!("creating {}", temp.display()))?;
        file.write_all(&serde_json::to_vec_pretty(state)?)?;
        file.sync_all()?;
        std::fs::rename(&temp, &self.path).with_context(|| format!("replacing {}", self.path.display()))?;
        let dir = self.path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
        File::open(dir).and_then(|dir| dir.sync_all()).context("syncing the state directory")?;
        Ok(())
    }
}

/// `path` with `suffix` appended to its whole file name, so it never aliases `path` itself.
pub(super) fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
