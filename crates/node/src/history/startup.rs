//! Authenticated startup plans contain only read-only verified data until external preflight ends.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tracing::warn;

use crate::authenticated::Store;
use crate::config::{HistorySection, SnapshotSection};
use crate::history::authenticated::{Head, Index};
use crate::history::rebuild::ReplayFingerprint;
use crate::history::runtime::{self, Replacement};
use crate::log::CheckpointDomain;
use crate::log::catalog::CatalogRoot;
use crate::log::catalog_access::{CatalogRead, StoreCatalogRead};
use crate::log::catalog_head::VerifiedCatalogHead;
use crate::log::catalog_head::{CatalogDomain, CatalogHeadAuthority};
use crate::log::catalog_owner::CatalogOwner;
use crate::sequencer::ReplayedLog;
use crate::snapshot::generations::Allocate;
use crate::snapshot::head::StoreId;
use crate::snapshot::lifecycle::Lifecycle;
use crate::snapshot::{Prune, catalog_heads, pins::Pins};
use crate::tee::EnclaveKey;

pub(crate) struct Plan {
    pub snapshots: SnapshotSection,
    pub history: HistorySection,
    pub log_dir: PathBuf,
    pub domain: CheckpointDomain,
    pub signer: Option<engine_core::types::AccountId>,
    pub source: Source,
    pub catalog: Catalog,
}

pub(crate) enum Source {
    Complete { id: StoreId, store: Store, head: Head },
    Rebuild { prefix: ReplayFingerprint },
}

pub(crate) enum Catalog {
    Complete { id: StoreId, store: Store, resume: Resume },
    Rebuild,
}

/// Where a complete catalog generation resumes writing.
pub(crate) enum Resume {
    /// The newest signed head; the replayed log holds everything it covers.
    Tip(Box<VerifiedCatalogHead>),
    /// The paired cache's own boundary, when no head verifies or the log lost a tail a head covered.
    Boundary(CatalogRoot),
}

impl Plan {
    /// Writes derived data only after the caller completes the on-chain checkpoint preflight (#24)
    /// and every other independent refusal check.
    /// Error cases:
    /// - verified allocation, same-prefix migration, retained pins or catalog owner activation fails
    pub(crate) fn activate(self, mut log: ReplayedLog, key: Arc<EnclaveKey>) -> Result<(ReplayedLog, super::History)> {
        let mut producer = self.snapshots.clone();
        let current_signer = engine_core::types::AccountId(key.address());
        if !producer.trusted_signers.contains(&current_signer) {
            producer.trusted_signers.push(current_signer);
        }
        // A running attested owner can protect its own writes; restart still uses original config pins.
        let mut owners = Vec::new();
        if let Source::Complete { id, store, .. } = &self.source {
            owners.push((*id, store.clone()));
        }
        if let Catalog::Complete { id, store, .. } = &self.catalog {
            owners.push((*id, store.clone()));
        }
        let active_catalog = match &self.catalog {
            Catalog::Complete { id, .. } => Some(*id),
            Catalog::Rebuild => None,
        };
        let lifecycle = Lifecycle::after_preflight(producer.clone(), self.domain, owners, active_catalog)?;
        if let Err(error) = crate::snapshot::reclaim(&producer, &mut lifecycle.publication()?) {
            warn!(
                alert = "snapshot_cleanup_failed",
                "interrupted cache publications remain until the next write: {error:#}"
            );
        }
        if let Catalog::Complete { id, resume: Resume::Boundary(boundary), .. } = &self.catalog {
            // Heads past the boundary name versions the catalog discards below. They go oldest first
            // and before history discards anything, so after a crash part way the next start still
            // sees the lost tail in the newest of them or in history's heads.
            let mut publication = lifecycle.publication()?;
            for path in catalog_heads::after(&producer, *id, boundary.root.version)? {
                publication.remove(&path)?;
            }
        }
        let mut allocated = if matches!(self.source, Source::Rebuild { .. }) {
            Some(
                lifecycle
                    .allocate(Allocate::Pair)?
                    .context("history generation allocation waits for actual old readers")?,
            )
        } else {
            None
        };
        let current = match self.source {
            Source::Complete { id, store, head } => {
                let mut pins = Pins::new(id, store.clone(), lifecycle.payloads());
                pins.refresh(&producer, self.domain)?;
                store.enable_writes()?;
                // Versions committed after the newest signed head were never acknowledged.
                store.discard_after(&head.root)?;
                Replacement { id, index: Index::resume(store.clone(), head)?, store }
            }
            Source::Rebuild { prefix } => {
                let generation = allocated
                    .as_mut()
                    .and_then(|pair| pair.history.take())
                    .context("fresh history generation missing")?;
                let mut index = Index::empty(
                    generation.store.clone(),
                    generation.id.history_namespace(),
                    self.history.retention_ms()?,
                )?;
                crate::sequencer::history_rebuild::rebuild_history_view(
                    &self.log_dir,
                    log.manifest_view(),
                    &prefix,
                    self.domain,
                    self.signer,
                    &mut index,
                )?;
                log.verify_history_tip(&index.head())?;
                Replacement { id: generation.id, store: generation.store, index }
            }
        };
        let (catalog_id, catalog_store, resume) = match self.catalog {
            Catalog::Complete { id, store, resume } => (id, store, Some(resume)),
            Catalog::Rebuild => {
                let generation = allocated
                    .as_mut()
                    .and_then(|pair| pair.catalog.take())
                    .context("fresh catalog generation missing")?;
                (generation.id, generation.store, None)
            }
        };
        lifecycle.active_catalog(catalog_id)?;
        let catalog_domain = CatalogDomain {
            chain_id: self.domain.chain_id,
            vault: self.domain.vault.0,
            log_id: catalog_id.catalog_namespace(),
        };
        let publication = catalog_heads::Publication::new(
            producer.clone(),
            catalog_id,
            catalog_domain.clone(),
            catalog_store.clone(),
            lifecycle.clone(),
        )?;
        let trust = catalog_heads::trust(&producer, catalog_domain);
        let authority = CatalogHeadAuthority::new(trust, key.clone(), publication)?;
        let probe = catalog_store.clone();
        let read: Arc<dyn CatalogRead> = Arc::new(StoreCatalogRead::new(
            Arc::new(catalog_store.clone()),
            Arc::new(move |root| probe.ensure_current(root)),
        ));
        catalog_store.enable_writes()?;
        let owner = match resume {
            Some(Resume::Tip(tip)) => {
                // Versions committed after the newest signed tip were never acknowledged.
                catalog_store.discard_after(&tip.catalog().root)?;
                CatalogOwner::resume(Arc::new(catalog_store), read, *tip, authority)?
            }
            Some(Resume::Boundary(boundary)) => {
                // Later versions hold what the log lost, or no head names them.
                catalog_store.discard_after(&boundary.root)?;
                CatalogOwner::resume_at_boundary(Arc::new(catalog_store), read, boundary, authority)?
            }
            None => CatalogOwner::create(
                Arc::new(catalog_store),
                read,
                crate::authenticated::Root::empty(catalog_id.catalog_namespace()),
                authority,
            )?,
        };
        log = if active_catalog.is_some() { log.bind_catalog(owner)? } else { log.attach_catalog(owner)? };
        let mut pins = Pins::new(current.id, current.store.clone(), lifecycle.payloads());
        pins.refresh(&producer, self.domain)?;
        let log_dir = self.log_dir;
        let domain = self.domain;
        let signer = self.signer;
        let repair_lifecycle = lifecycle.clone();
        let rebuild: runtime::Rebuild = Box::new(move |expected, stopped| {
            rebuild_generation(&repair_lifecycle, &log_dir, domain, signer, expected, stopped)
        });
        let config = runtime::Config { snapshots: producer, domain, key, lifecycle, catalog_store: catalog_id };
        let runtime = runtime::Runtime::new(current, pins, config, rebuild)?;
        Ok((log, super::History::authenticated(runtime)))
    }
}

/// Waits only on real reader lifetimes; cancellation never drops their pins or discards evidence.
/// Error cases:
/// - allocation, strict log-derived rebuilding, cancellation or exact rebuilt-state validation fails
fn rebuild_generation(
    lifecycle: &Lifecycle,
    log_dir: &std::path::Path,
    domain: CheckpointDomain,
    signer: Option<engine_core::types::AccountId>,
    expected: &Head,
    stopped: &AtomicBool,
) -> Result<Replacement> {
    let mut pair = loop {
        ensure!(!stopped.load(Ordering::Acquire), "history repair cancelled");
        if let Some(pair) = lifecycle.allocate(Allocate::History)? {
            break pair;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let generation = pair.history.take().context("replacement history generation missing")?;
    let mut index = Index::empty(generation.store.clone(), generation.id.history_namespace(), expected.retention_ms)?;
    if let Some(seq) = expected.last_seq() {
        crate::sequencer::history_rebuild::rebuild_history_prefix(
            log_dir,
            domain,
            signer,
            (seq, expected.last_ts().context("complete repair head lacks time")?, expected.command_prefix),
            &mut index,
            stopped,
        )?;
    }
    let mut comparable = expected.clone();
    comparable.root = index.head().root;
    ensure!(comparable == index.head(), "fresh history generation differs from the complete source prefix");
    Ok(Replacement { id: generation.id, store: generation.store, index })
}
