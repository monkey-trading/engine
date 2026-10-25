//! Immutable exact-C capture for a causally ordered asynchronous storage-owner request.

use std::sync::Arc;

use anyhow::{Context, ensure};

use crate::log::catalog::{CheckpointLocator, LazyCatalogView};
use crate::log::catalog_head::CatalogDomain;
use crate::log::catalog_owner::CatalogOwner;
use crate::log::{ActivePrefix, CatalogBoundary, LogWriter};

/// An owned physical/logical C fence; it is not a durable catalog or history acknowledgment.
pub struct PendingCatalogSeal {
    domain: CatalogDomain,
    view: Arc<LazyCatalogView>,
    checkpoint: CheckpointLocator,
    prefix: ActivePrefix,
    manifest_seek: u64,
}

impl PendingCatalogSeal {
    /// Returns the exact physical checkpoint identity used to correlate the engine/history fence.
    pub fn checkpoint(&self) -> &CheckpointLocator {
        &self.checkpoint
    }

    /// Returns the actual flushed-byte commitment captured at C.
    pub fn physical_prefix(&self) -> &ActivePrefix {
        &self.prefix
    }

    /// Commits this fence before processing later catalog folds in the same serialized owner.
    /// Error cases:
    /// - the owner already passed this exact prefix, or proof/commit/publication fails
    pub fn commit(&self, owner: &mut CatalogOwner) -> anyhow::Result<(CatalogBoundary, Arc<LazyCatalogView>)> {
        ensure!(owner.domain() == &self.domain, "catalog seal deployment mismatch");
        let catalog = Arc::new(self.view.fold(owner, Some(self.checkpoint.clone()))?);
        ensure!(
            catalog.certified().completed_segments == self.view.len()
                && catalog.certified().manifest_bytes == self.view.manifest_bytes()
                && catalog.certified().prefix == self.view.prefix(),
            "catalog seal went past its checkpoint"
        );
        let boundary = CatalogBoundary {
            domain: self.domain.clone(),
            catalog: catalog.certified().clone(),
            checkpoint: self.checkpoint.clone(),
            prefix: self.prefix.clone(),
            manifest_seek: self.manifest_seek,
        };
        Ok((boundary, catalog))
    }
}

impl LogWriter {
    /// Captures synced bytes before the flush frontier is made visible to disk backfill.
    /// Error cases:
    /// - no exact active checkpoint, missing catalog or missing serialized write ownership
    pub(crate) fn capture_catalog_boundary_unpublished(&self) -> anyhow::Result<PendingCatalogSeal> {
        let view = self.manifest.borrow().clone();
        let (segment, _) = self.open.as_ref().context("checkpoint has no active segment")?;
        let last = self.last.context("checkpoint has no physical offset")?;
        let prefix = ActivePrefix { info: segment.info(), last };
        let checkpoint = segment.checkpoints.last().context("checkpoint locator missing")?.clone();
        ensure!(
            checkpoint.offset == last && checkpoint.end == prefix.info.bytes,
            "frontier is not an exact checkpoint"
        );
        let domain = self.catalog_owner.as_ref().context("catalog owner missing")?.domain().clone();
        Ok(PendingCatalogSeal {
            domain,
            view: view.catalog.context("checkpoint has no authenticated catalog")?,
            checkpoint,
            prefix,
            manifest_seek: self.manifest_len,
        })
    }

    /// Captures C after authoritative sync, without waiting for history or mutating the Store.
    /// Register its storage-owner request before any later catalog update; an unsatisfied attempt skips caching.
    /// Error cases:
    /// - the flushed physical frontier is not an exact checkpoint or has no authenticated catalog
    pub fn capture_catalog_boundary(&self) -> anyhow::Result<PendingCatalogSeal> {
        let view = self.manifest.borrow().clone();
        let prefix = view.active_prefix()?.context("checkpoint has no active prefix")?;
        ensure!(Some(prefix.last) == self.last && view.flushed == self.last, "checkpoint is not the flushed frontier");
        let (segment, _) = self.open.as_ref().context("checkpoint has no segment")?;
        let checkpoint = segment.checkpoints.last().context("checkpoint locator missing")?.clone();
        ensure!(
            checkpoint.offset == prefix.last && checkpoint.end == prefix.info.bytes,
            "frontier is not an exact checkpoint"
        );
        let domain = self.catalog_owner.as_ref().context("catalog owner missing")?.domain().clone();
        Ok(PendingCatalogSeal {
            domain,
            view: view.catalog.context("checkpoint has no authenticated catalog")?,
            checkpoint,
            prefix,
            manifest_seek: self.manifest_len,
        })
    }
}
