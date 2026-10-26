//! Bounded catalog-head publication preserves stable certificates and their actual root leases.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};

use crate::authenticated::{Root, Store};
use crate::config::SnapshotSection;
use crate::log::CheckpointDomain;
use crate::log::catalog_head::{
    CatalogDomain, CatalogHeadCertificate, CatalogHeadPublication, CatalogTrust, VerifiedCatalogHead,
};
use crate::snapshot::head::StoreId;
use crate::snapshot::lifecycle::Lifecycle;
use crate::snapshot::pins::Pins;
use crate::snapshot::{publish, reconcile_managed, sync_directory};

const RETAINED_HEADS: usize = 2;

/// A single serialized catalog owner invokes the shared callback; the mutex makes pins Sync.
pub(crate) struct Publication {
    cfg: SnapshotSection,
    id: StoreId,
    trust: CatalogTrust,
    pins: Mutex<Pins>,
    lifecycle: Arc<Lifecycle>,
}

impl Publication {
    /// Restores all retained references on this same Store before the caller enables writes.
    /// Error cases:
    /// - any retained signed reference or root lease is unavailable
    pub(crate) fn new(
        cfg: SnapshotSection,
        id: StoreId,
        domain: CatalogDomain,
        store: Store,
        lifecycle: Arc<Lifecycle>,
    ) -> Result<Arc<Self>> {
        let trust = trust(&cfg, domain);
        let mut pins = Pins::new(id, store, lifecycle.payloads());
        pins.refresh(
            &cfg,
            CheckpointDomain {
                chain_id: trust.domain.chain_id,
                vault: engine_core::types::AccountId(trust.domain.vault),
            },
        )?;
        Ok(Arc::new(Self { cfg, id, trust, pins: Mutex::new(pins), lifecycle }))
    }
}

impl CatalogHeadPublication for Publication {
    /// Uses the publication lock order and keeps old pins if any metadata read is incomplete.
    /// Error cases:
    /// - lifecycle ownership, metadata authentication or version leasing fails
    fn prepare_maintenance(&self) -> Result<()> {
        let mut publication = self.lifecycle.publication()?;
        publication.settle()?;
        let mut pins = self.pins.lock().map_err(|_| anyhow::anyhow!("catalog cache pins poisoned"))?;
        pins.refresh(
            &self.cfg,
            CheckpointDomain {
                chain_id: self.trust.domain.chain_id,
                vault: engine_core::types::AccountId(self.trust.domain.vault),
            },
        )
    }

    /// Pins data before allocation and retains uncertain post-rename publications for later retry.
    /// Error cases:
    /// - head proof identity, staging, file/directory sync, pruning or pin refresh fails
    fn publish(&self, previous: Option<&CatalogHeadCertificate>, next: &CatalogHeadCertificate) -> Result<()> {
        let verified = next.verify(&self.trust)?;
        ensure!(
            verified.catalog().root.namespace == self.id.catalog_namespace(),
            "catalog publication namespace mismatch"
        );
        if let Some(previous) = previous {
            previous.verify(&self.trust)?;
            ensure!(previous.body.nonce <= next.body.nonce, "catalog publication nonce regressed");
        }
        let mut publication = self.lifecycle.publication()?;
        publication.settle()?;
        let mut pins = self.pins.lock().map_err(|_| anyhow::anyhow!("catalog cache pins poisoned"))?;
        pins.refresh(
            &self.cfg,
            CheckpointDomain {
                chain_id: self.trust.domain.chain_id,
                vault: engine_core::types::AccountId(self.trust.domain.vault),
            },
        )?;
        pins.protect(&verified.catalog().root)?;
        let group = |file: &str| belongs(self.id, file);
        reconcile_managed(&self.cfg.dir, RETAINED_HEADS, group, &sync_directory, &mut publication)?;
        let bytes = serde_json::to_vec(next)?;
        ensure!(bytes.len() <= self.cfg.max_bytes.get(), "catalog head exceeds snapshots.max_bytes");
        fs::create_dir_all(&self.cfg.dir)?;
        let name = name(self.id, next.body.nonce);
        publish(&self.cfg.dir.join(format!("{name}.tmp")), &self.cfg.dir.join(name), |file| {
            file.write_all(&bytes).context("writing catalog head")?;
            file.sync_all().context("syncing catalog head")
        })?;
        sync_directory(&self.cfg.dir)?;
        reconcile_managed(&self.cfg.dir, RETAINED_HEADS, group, &sync_directory, &mut publication)?;
        pins.refresh(
            &self.cfg,
            CheckpointDomain {
                chain_id: self.trust.domain.chain_id,
                vault: engine_core::types::AccountId(self.trust.domain.vault),
            },
        )?;
        Ok(())
    }

    /// Returns a bounded independently verified slot without opening any writable file.
    /// Error cases:
    /// - candidate IO/authentication or encoding fails
    fn read(&self) -> Result<Option<Vec<u8>>> {
        let paths = candidates(&self.cfg, self.id)?;
        if paths.is_empty() {
            return Ok(None);
        }
        for path in paths {
            match read_one(&self.cfg, self.id, &self.trust, &path) {
                Ok(head) => return Ok(Some(serde_json::to_vec(head.certificate())?)),
                Err(error) => crate::snapshot::rejected(&path, &error),
            }
        }
        anyhow::bail!("no verified catalog head slot")
    }
}

/// Uses independent restart pins and the generation identity authenticated by the paired cache;
/// `None` when no head verifies, each rejected one with a Warning.
/// Error cases:
/// - the cache directory cannot be listed
pub(crate) fn read_latest(
    cfg: &SnapshotSection,
    id: StoreId,
    domain: &CatalogDomain,
) -> Result<Option<VerifiedCatalogHead>> {
    let trust = trust(cfg, domain.clone());
    for path in candidates(cfg, id)? {
        match read_one(cfg, id, &trust, &path) {
            Ok(head) => return Ok(Some(head)),
            Err(error) => crate::snapshot::rejected(&path, &error),
        }
    }
    Ok(None)
}

/// The generation's head files numbered after `version`, verified or not, oldest first.
/// Error cases:
/// - the cache directory cannot be listed
pub(crate) fn after(cfg: &SnapshotSection, id: StoreId, version: Option<u64>) -> Result<Vec<PathBuf>> {
    let mut paths = candidates(cfg, id)?;
    paths.retain(|path| nonce(path) > version);
    paths.reverse();
    Ok(paths)
}

/// Enumerates only configured metadata slots, never nodes, history rows or segment ordinals.
/// Error cases:
/// - candidate listing, authentication or bounded metadata decoding fails
pub(super) fn retained_roots(cfg: &SnapshotSection, id: StoreId, domain: CheckpointDomain) -> Result<Vec<Root>> {
    let catalog_domain =
        CatalogDomain { chain_id: domain.chain_id, vault: domain.vault.0, log_id: id.catalog_namespace() };
    let trust = trust(cfg, catalog_domain);
    let mut roots = Vec::new();
    for path in candidates(cfg, id)? {
        match read_one(cfg, id, &trust, &path) {
            Ok(head) => roots.push(head.catalog().root.clone()),
            Err(error) => crate::snapshot::pins::rejected_reference(&path, error)?,
        }
    }
    Ok(roots)
}

pub(crate) fn trust(cfg: &SnapshotSection, domain: CatalogDomain) -> CatalogTrust {
    CatalogTrust { domain, signers: cfg.trusted_signers.iter().map(|signer| signer.0).collect::<BTreeSet<_>>() }
}

pub(super) fn read_one(
    cfg: &SnapshotSection,
    id: StoreId,
    trust: &CatalogTrust,
    path: &Path,
) -> Result<VerifiedCatalogHead> {
    let mut bytes = Vec::new();
    File::open(path)?.take((cfg.max_bytes.get() as u64).saturating_add(1)).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= cfg.max_bytes.get(), "catalog head exceeds snapshots.max_bytes");
    let certificate: CatalogHeadCertificate = serde_json::from_slice(&bytes)?;
    let head = certificate.verify(trust)?;
    ensure!(head.catalog().root.namespace == id.catalog_namespace(), "catalog head namespace mismatch");
    ensure!(
        path.file_name().and_then(|file| file.to_str()) == Some(name(id, certificate.body.nonce).as_str()),
        "catalog head filename mismatch"
    );
    Ok(head)
}

/// Supplies candidate IDs only; each certificate is independently verified before it protects data.
/// Error cases:
/// - a complete cache-directory enumeration fails
pub(super) fn all_candidates(cfg: &SnapshotSection) -> Result<Vec<(StoreId, PathBuf)>> {
    let entries = match fs::read_dir(&cfg.dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file = entry.file_name();
        let Some(id) = file
            .to_str()
            .and_then(|name| name.strip_prefix("catalog-head-"))
            .and_then(|tail| tail.get(..64))
            .and_then(|text| hex::decode(text).ok())
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .map(StoreId)
        else {
            continue;
        };
        if file.to_str().is_some_and(|name| belongs(id, name)) {
            paths.push((id, entry.path()));
        }
    }
    paths.sort_unstable_by(|left, right| left.1.cmp(&right.1));
    Ok(paths)
}

/// Removes every recognized catalog-head staging file, including those of generations no longer
/// in use. Startup calls this before any catalog publication can be in flight.
/// Error cases:
/// - the cache directory cannot be listed, a staging file cannot be removed, or the directory sync fails
pub(super) fn reclaim_staging(cfg: &SnapshotSection) -> Result<()> {
    let entries = match fs::read_dir(&cfg.dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut removed = false;
    for entry in entries {
        let entry = entry?;
        let staged = entry.file_name().to_str().and_then(|name| name.strip_suffix(".tmp")).is_some_and(|name| {
            name.strip_prefix("catalog-head-")
                .and_then(|tail| tail.get(..64))
                .and_then(|text| hex::decode(text).ok())
                .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
                .is_some_and(|id| belongs(StoreId(id), name))
        });
        if staged {
            fs::remove_file(entry.path()).context("removing interrupted catalog head staging file")?;
            removed = true;
        }
    }
    if removed {
        sync_directory(&cfg.dir)?;
    }
    Ok(())
}

fn candidates(cfg: &SnapshotSection, id: StoreId) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(&cfg.dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_str().is_some_and(|name| belongs(id, name)) {
            paths.push(entry.path());
        }
    }
    paths.sort_unstable_by(|left, right| right.cmp(left));
    Ok(paths)
}

/// The nonce a head file is named for.
fn nonce(path: &Path) -> Option<u64> {
    path.file_name()?.to_str()?.strip_suffix(".json")?.rsplit('-').next()?.parse().ok()
}

fn name(id: StoreId, nonce: u64) -> String {
    format!("catalog-head-{}-{nonce:020}.json", hex::encode(id.0))
}

fn belongs(id: StoreId, name: &str) -> bool {
    name.strip_prefix(&format!("catalog-head-{}-", hex::encode(id.0)))
        .and_then(|value| value.strip_suffix(".json"))
        .is_some_and(|digits| digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit()))
}
