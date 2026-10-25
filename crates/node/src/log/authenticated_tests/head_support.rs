//! Real certificate slots and read-only probes for the catalog continuation tests.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::authenticated::{Root, Store};
use crate::log::catalog_access::{CatalogRead, StoreCatalogRead};
use crate::log::catalog_head::{
    CatalogDomain, CatalogHeadAuthority, CatalogHeadCertificate, CatalogHeadPublication, CatalogTrust,
    VerifiedCatalogHead, head_digest, read_catalog_head,
};
use crate::log::catalog_owner::CatalogOwner;
use crate::tee::EnclaveKey;

/// Sees each final head before its publication is acknowledged.
type Observer = Arc<dyn Fn(&CatalogHeadCertificate) -> anyhow::Result<()> + Send + Sync>;

pub(crate) struct HeadFiles {
    pub(super) dir: PathBuf,
    mode: AtomicU8,
    staged: AtomicU64,
    observations: Mutex<Vec<CatalogHeadCertificate>>,
    observer: Mutex<Option<Observer>>,
}

impl HeadFiles {
    /// Uses ordinary test files; no process-global state or private-key persistence.
    pub(super) fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
            mode: AtomicU8::new(0),
            staged: AtomicU64::new(0),
            observations: Mutex::new(Vec::new()),
            observer: Mutex::new(None),
        }
    }

    /// Injects one publication phase failure: 1 before writing, 2 after file sync, 3 after rename.
    pub(super) fn fail_at(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    /// Observes the final logical head before publication ack while the consumer view is still old.
    pub(super) fn observe(&self, observer: Observer) {
        *self.observer.lock().expect("observer lock") = Some(observer);
    }

    /// Returns successful certificate attempts for decisive count/root assertions.
    pub(super) fn publications(&self) -> Vec<CatalogHeadCertificate> {
        self.observations.lock().expect("observations lock").clone()
    }

    /// Writes and syncs a new private stage using exclusive creation.
    fn stage(&self, bytes: &[u8]) -> anyhow::Result<PathBuf> {
        let id = self.staged.fetch_add(1, Ordering::SeqCst);
        let path = self.dir.join(format!("catalog-head-{id}.tmp"));
        let mut file = OpenOptions::new().write(true).create_new(true).open(&path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(path)
    }
}

impl CatalogHeadPublication for HeadFiles {
    /// Preserves the previously acknowledged signed head before atomic latest-slot replacement.
    /// Error cases:
    /// - any injected phase, encoding, write, rename, file sync or directory sync fails
    fn publish(&self, previous: Option<&CatalogHeadCertificate>, next: &CatalogHeadCertificate) -> anyhow::Result<()> {
        ensure!(self.mode.load(Ordering::SeqCst) != 1, "injected head failure before write");
        if let Some(observer) = self.observer.lock().expect("observer lock").clone() {
            observer(next)?;
        }
        let dir = File::open(&self.dir)?;
        if let Some(previous) = previous {
            let staged = self.stage(&serde_json::to_vec(previous)?)?;
            fs::rename(staged, self.dir.join("catalog-head.previous.json"))?;
            dir.sync_all()?;
        }
        let staged = self.stage(&serde_json::to_vec(next)?)?;
        ensure!(self.mode.load(Ordering::SeqCst) != 2, "injected head failure after file sync");
        fs::rename(staged, self.dir.join("catalog-head.json"))?;
        ensure!(self.mode.load(Ordering::SeqCst) != 3, "injected head failure before directory sync");
        dir.sync_all()?;
        self.observations.lock().expect("observations lock").push(next.clone());
        Ok(())
    }

    /// Reads only a bounded fixed slot; absent or oversized heads are cache faults.
    /// Error cases:
    /// - the head is unavailable or malformed at the IO boundary
    fn read(&self) -> anyhow::Result<Option<Vec<u8>>> {
        use std::io::Read;
        let file = match File::open(self.dir.join("catalog-head.json")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 16 * 1024, "oversized head file");
        Ok(Some(bytes))
    }
}

/// The domain comes from the test's independently provisioned deployment context.
pub(super) fn trust() -> CatalogTrust {
    CatalogTrust {
        domain: CatalogDomain { chain_id: 31337, vault: [2; 20], log_id: [3; 32] },
        signers: [EnclaveKey::test_key().address()].into(),
    }
}

/// Supplies the node's existing public mock enclave authority to a real publication callback.
pub(super) fn authority(files: Arc<HeadFiles>) -> CatalogHeadAuthority {
    CatalogHeadAuthority::new(trust(), Arc::new(EnclaveKey::test_key()), files).expect("pinned test authority")
}

/// Performs a bounded SQL successor refusal probe, never deriving a trusted head from SQL rows.
pub(super) fn access(store: Arc<Store>, path: PathBuf) -> Arc<dyn CatalogRead> {
    Arc::new(StoreCatalogRead::new(
        store,
        Arc::new(move |root| {
            let conn =
                Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
            let later: Option<i64> = conn
                .query_row(
                    "SELECT version FROM nodes WHERE namespace=?1 AND version>?2 ORDER BY version LIMIT 1",
                    params![&root.namespace[..], root.version.map_or(-1, |v| v as i64)],
                    |row| row.get(0),
                )
                .optional()?;
            ensure!(later.is_none(), "unacknowledged Store successor; cache fault fallback");
            Ok(())
        }),
    ))
}

/// Reads the latest signed tip with independent signer/domain authentication.
pub(super) fn latest(files: &HeadFiles) -> VerifiedCatalogHead {
    read_catalog_head(files, &trust()).expect("latest durable head")
}

/// Attaches writable ownership after the test's read-only replay/preflight boundary.
pub(super) fn writable(
    log: crate::log::SegmentedLog,
    store: Arc<Store>,
    read: Arc<dyn CatalogRead>,
    files: Arc<HeadFiles>,
) -> crate::log::SegmentedLog {
    let owner =
        CatalogOwner::resume(store, read, latest(&files), authority(files)).expect("authorized current tip owner");
    log.bind_catalog_owner(owner).expect("bind proven continuation")
}

/// Signs a deliberately altered fixture body using the existing pinned test signer.
pub(super) fn resign(mut certificate: CatalogHeadCertificate) -> CatalogHeadCertificate {
    certificate.signature = EnclaveKey::test_key().sign_digest(head_digest(&certificate.body).expect("fixture digest"));
    certificate
}

/// Represents an official verified maintenance transition whose exact changes touch no catalog key.
/// This exercises root-version handoff, not the sibling's still evolving persistent-GC algorithm.
pub(super) fn maintenance_root(store: &Store, root: &Root) -> anyhow::Result<Root> {
    let next = root.version.context("maintenance base")?.checked_add(1).context("maintenance version")?;
    store.update(root, vec![(b"\0store-test/retirement-cursor".to_vec(), Some(next.to_be_bytes().to_vec()))])
}
