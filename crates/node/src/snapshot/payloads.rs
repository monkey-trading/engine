//! Authenticated cache payloads, rejections, and inventory records reused while their file is
//! unchanged. Retention bookkeeping reads every retained cache on each history batch and catalog
//! publication, the sequencer's at each checkpoint included; re-reading, re-hashing and re-verifying
//! whole engine states each time would grow with engine state. Recovery never uses this reuse: it
//! reads and authenticates every candidate it selects.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use engine_core::types::AccountId;

use crate::config::SnapshotSection;
use crate::snapshot::references::Record;
use crate::snapshot::{Snapshot, candidates_readonly, decode_payload, pins, verified_payload};

/// One version of a file: rewriting it changes its change time, and replacing it changes its inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

/// What one read of a cache file established.
#[derive(Clone)]
pub(crate) enum Cached {
    /// Its authenticated payload, without engine state.
    Payload(Arc<Snapshot>),
    /// A signed engine-only cache in a form this code no longer decodes; it holds no generation.
    EngineOnly,
    /// Unauthenticated: reported once when read, then skipped.
    Rejected,
    /// A signed payload in another form, which may name a generation this code cannot read.
    Unsupported(Arc<str>),
}

/// One version of a cache file, what reading it established, and the inventory record derived
/// from it, once derived.
struct Entry {
    identity: Identity,
    cached: Cached,
    record: Option<Option<Record>>,
}

/// Each retained cache, newest first, with what its current version established, or an IO error.
pub(crate) type Listed = Vec<(PathBuf, Result<Cached>)>;

/// Reusable outcomes per retained cache path.
#[derive(Default)]
pub(crate) struct Payloads {
    entries: Mutex<BTreeMap<PathBuf, Entry>>,
}

impl Payloads {
    /// Lists the retained caches newest first with what each file version established. A file is
    /// read once per version: an IO error is returned and never kept; a rejection is reported when
    /// first read. Caches no longer listed are forgotten.
    /// Error cases:
    /// - the cache directory cannot be listed, or the reuse table is poisoned
    pub(crate) fn verified(&self, cfg: &SnapshotSection, chain_id: u64, vault: AccountId) -> Result<Listed> {
        let paths = candidates_readonly(cfg)?;
        let mut entries = self.entries.lock().map_err(|_| anyhow::anyhow!("snapshot payload table poisoned"))?;
        let mut kept = BTreeMap::new();
        let mut listed = Vec::with_capacity(paths.len());
        for path in paths {
            let identity = match identity(&path) {
                Ok(identity) => identity,
                Err(error) => {
                    listed.push((path, Err(error)));
                    continue;
                }
            };
            if let Some(entry) = entries.remove(&path)
                && entry.identity == identity
            {
                listed.push((path.clone(), Ok(entry.cached.clone())));
                kept.insert(path, entry);
                continue;
            }
            let cached = match read(cfg, &path, chain_id, vault) {
                Ok(cached) => cached,
                Err(error) => {
                    listed.push((path, Err(error)));
                    continue;
                }
            };
            listed.push((path.clone(), Ok(cached.clone())));
            kept.insert(path, Entry { identity, cached, record: None });
        }
        *entries = kept;
        Ok(listed)
    }

    /// Records the payload this node just signed and renamed to `path`, so pruning after the
    /// rename does not read the whole cache back. A file whose identity cannot be read is left for
    /// the next listing to read.
    pub(crate) fn seed(&self, path: &Path, mut snapshot: Snapshot) {
        snapshot.engine = String::new();
        if let (Ok(identity), Ok(mut entries)) = (identity(path), self.entries.lock()) {
            entries
                .insert(path.to_owned(), Entry { identity, cached: Cached::Payload(Arc::new(snapshot)), record: None });
        }
    }

    /// The inventory record of the cache at `path` as `verified` last listed it, derived once per
    /// file version; an error is returned and not kept.
    /// Error cases:
    /// - `derive` fails, or the reuse table is poisoned
    pub(super) fn record(
        &self,
        path: &Path,
        derive: impl FnOnce() -> Result<Option<Record>>,
    ) -> Result<Option<Record>> {
        let mut entries = self.entries.lock().map_err(|_| anyhow::anyhow!("snapshot payload table poisoned"))?;
        if let Some(record) = entries.get(path).and_then(|entry| entry.record.clone()) {
            return Ok(record);
        }
        let record = derive()?;
        if let Some(entry) = entries.get_mut(path) {
            entry.record = Some(record.clone());
        }
        Ok(record)
    }
}

/// Reads and classifies one cache file version in a single read.
/// Error cases:
/// - IO fails
fn read(cfg: &SnapshotSection, path: &Path, chain_id: u64, vault: AccountId) -> Result<Cached> {
    let (payload, signer) = match verified_payload(cfg, path, chain_id, vault) {
        Ok(verified) => verified,
        Err(error) if pins::is_io(&error) => return Err(error),
        Err(error) => {
            crate::snapshot::rejected(path, &error);
            return Ok(Cached::Rejected);
        }
    };
    Ok(match decode_payload(path, chain_id, vault, &payload, signer) {
        Ok(mut snapshot) => {
            snapshot.engine = String::new();
            Cached::Payload(Arc::new(snapshot))
        }
        Err(error) => {
            let value: Option<serde_json::Value> = serde_json::from_str(&payload).ok();
            let engine_only = value.as_ref().is_some_and(|value| {
                matches!(value.get("schema").and_then(serde_json::Value::as_u64), Some(1 | 2))
                    && value.get("pair").is_none_or(serde_json::Value::is_null)
            });
            if engine_only { Cached::EngineOnly } else { Cached::Unsupported(Arc::from(format!("{error:#}"))) }
        }
    })
}

/// Reads the identity of the file `read` opens at `path`.
/// Error cases:
/// - the file cannot be inspected
fn identity(path: &Path) -> Result<Identity> {
    let metadata = std::fs::metadata(path)?;
    Ok(Identity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        len: metadata.len(),
        mtime: (metadata.mtime(), metadata.mtime_nsec()),
        ctime: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::tests::fixture;
    use crate::snapshot::write;

    /// A rewritten cache is authenticated again, and a removed one leaves the table.
    #[test]
    fn reuse_ends_when_a_cache_file_changes_or_disappears() {
        let (cfg, key, snapshot) = fixture("snapshot-payload-reuse");
        write(&cfg, &snapshot, &key).unwrap();
        let payloads = Payloads::default();
        let first = payloads.verified(&cfg, snapshot.chain_id, snapshot.vault).unwrap();
        let (path, cached) = &first[0];
        let Ok(Cached::Payload(cached)) = cached else { panic!("authenticated") };
        assert!(cached.engine.is_empty(), "engine state is not retained");
        let again = payloads.verified(&cfg, snapshot.chain_id, snapshot.vault).unwrap();
        let Ok(Cached::Payload(again)) = &again[0].1 else { panic!("still authenticated") };
        assert!(Arc::ptr_eq(cached, again), "an unchanged file is not re-read");

        let mut bytes = std::fs::read(path).unwrap();
        let last = bytes.len() - 3;
        bytes[last] ^= 1;
        std::fs::write(path, &bytes).unwrap();
        let tampered = payloads.verified(&cfg, snapshot.chain_id, snapshot.vault).unwrap();
        assert!(matches!(tampered[0].1, Ok(Cached::Rejected)), "a changed file is authenticated again");

        std::fs::remove_file(path).unwrap();
        assert!(payloads.verified(&cfg, snapshot.chain_id, snapshot.vault).unwrap().is_empty());
        assert!(payloads.entries.lock().unwrap().is_empty(), "removed caches are forgotten");
    }

    /// The payload the owner just signed and renamed is recorded, so pruning does not read it back.
    #[test]
    fn a_seeded_cache_version_is_not_read_back() {
        let (cfg, key, snapshot) = fixture("snapshot-payload-seed");
        write(&cfg, &snapshot, &key).unwrap();
        let path = candidates_readonly(&cfg).unwrap().remove(0);
        let (seeded, _) = crate::snapshot::read(&cfg, &path, snapshot.chain_id, snapshot.vault).unwrap();
        // Read back, these bytes would be rejected.
        std::fs::write(&path, b"not a cache").unwrap();
        let payloads = Payloads::default();
        payloads.seed(&path, seeded);
        let listed = payloads.verified(&cfg, snapshot.chain_id, snapshot.vault).unwrap();
        assert!(matches!(listed[0].1, Ok(Cached::Payload(_))), "the seeded version is taken as recorded");
    }

    /// Inventory, pins and catalog references all list a rejected cache, which is read and
    /// reported once per file version.
    #[test]
    fn a_rejected_cache_is_read_and_reported_once_per_file_version() {
        let (cfg, key, snapshot) = fixture("snapshot-rejected-once");
        write(&cfg, &snapshot, &key).unwrap();
        std::fs::write(cfg.dir.join("99999999999999999999-0000000000.json"), b"{\"payload\":").unwrap();
        let payloads = Arc::new(Payloads::default());
        let domain = crate::log::CheckpointDomain { chain_id: snapshot.chain_id, vault: snapshot.vault };
        let (_guard, mut alerts) = crate::test_support::capture_alerts();
        let mut inventory =
            crate::snapshot::references::Inventory::open(cfg.clone(), domain, None, payloads.clone()).unwrap();
        inventory.verify(&[]).unwrap();
        crate::snapshot::pins::catalog_references(&cfg, domain, &payloads).unwrap();
        let reports =
            std::iter::from_fn(|| alerts.try_recv().ok()).filter(|alert| alert["alert"] == "snapshot_rejected").count();
        assert_eq!(reports, 1);
    }

    /// A rejected cache and a verified one each yield their inventory record once per file
    /// version; only a changed file is classified again.
    #[test]
    fn records_are_derived_once_per_file_version_rejected_caches_included() {
        let (cfg, key, snapshot) = fixture("snapshot-record-reuse");
        write(&cfg, &snapshot, &key).unwrap();
        let rejected = cfg.dir.join("99999999999999999999-0000000000.json");
        std::fs::write(&rejected, b"{\"payload\":").unwrap();
        let payloads = Payloads::default();
        let listed = payloads.verified(&cfg, snapshot.chain_id, snapshot.vault).unwrap();
        assert!(matches!(listed[0].1, Ok(Cached::Rejected)), "the corrupt cache is rejected, not an IO error");
        for (path, _) in &listed {
            payloads.record(path, || Ok(None)).unwrap();
        }
        payloads.verified(&cfg, snapshot.chain_id, snapshot.vault).unwrap();
        for (path, _) in &listed {
            payloads.record(path, || panic!("re-derived an unchanged file: {}", path.display())).unwrap();
        }
        std::fs::write(&rejected, b"{\"payload\":\"\"").unwrap();
        payloads.verified(&cfg, snapshot.chain_id, snapshot.vault).unwrap();
        let mut derived = false;
        payloads
            .record(&rejected, || {
                derived = true;
                Ok(None)
            })
            .unwrap();
        assert!(derived, "a changed file is classified again");
    }
}
