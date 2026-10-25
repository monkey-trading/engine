//! Serialized post-preflight catalog writes, maintenance rebases and signed head publication.

use std::sync::Arc;

use anyhow::{Context, ensure};

use crate::authenticated::{Root, Store};
use crate::log::catalog::{CatalogMeta, CatalogRoot, LazyCatalogView, META};
use crate::log::catalog_access::CatalogRead;
use crate::log::catalog_head::{CatalogHeadAuthority, VerifiedCatalogHead};

pub struct CatalogOwner {
    pub(super) store: Arc<Store>,
    pub(super) read: Arc<dyn CatalogRead>,
    pub(super) current: Root,
    authority: CatalogHeadAuthority,
    stable: LazyCatalogView,
    pending: Option<LazyCatalogView>,
    failed: bool,
}

impl CatalogOwner {
    /// Creates a fresh derived namespace after recovery preflight permits cache writes.
    /// Error cases:
    /// - namespace is not empty, initial exact commit fails, or its signed head is not durable
    pub fn create(
        store: Arc<Store>,
        read: Arc<dyn CatalogRead>,
        empty: Root,
        mut authority: CatalogHeadAuthority,
    ) -> anyhow::Result<Self> {
        ensure!(empty.version.is_none(), "catalog namespace is not empty");
        ensure!(read.read(&empty, META)?.is_none(), "catalog metadata already exists");
        let meta = CatalogMeta { completed_segments: 0, manifest_bytes: 0, next_seq: 0, prefix: [0; 32] };
        let root = store.update(&empty, vec![(META.to_vec(), Some(serde_json::to_vec(&meta)?))])?;
        let stable = LazyCatalogView::open(read.clone(), root_with_meta(root.clone(), &meta))?;
        authority.publish(stable.certified().clone())?;
        Ok(Self { store, read, current: root, authority, stable, pending: None, failed: false })
    }

    /// Binds an already verified clean current tip to writable Store ownership after #24 preflight.
    /// Error cases:
    /// - tip domain, current-version probe, root proof, or authority restoration fails
    pub fn resume(
        store: Arc<Store>,
        read: Arc<dyn CatalogRead>,
        head: VerifiedCatalogHead,
        mut authority: CatalogHeadAuthority,
    ) -> anyhow::Result<Self> {
        read.ensure_current(&head.catalog().root)?;
        let stable = LazyCatalogView::open(read.clone(), head.catalog().clone())?;
        let current = head.catalog().root.clone();
        authority.restore(head)?;
        Ok(Self { store, read, current, authority, stable, pending: None, failed: false })
    }

    /// Binds a paired cache's boundary root, which that cache's signature authenticates, to writable
    /// ownership after #24 preflight. The caller has removed the heads past it and discarded the
    /// versions they named; the next fold signs the next head.
    /// Error cases:
    /// - a later version remains, or the root proof fails
    pub fn resume_at_boundary(
        store: Arc<Store>,
        read: Arc<dyn CatalogRead>,
        boundary: CatalogRoot,
        authority: CatalogHeadAuthority,
    ) -> anyhow::Result<Self> {
        read.ensure_current(&boundary.root)?;
        let current = boundary.root.clone();
        let stable = LazyCatalogView::open(read.clone(), boundary)?;
        Ok(Self { store, read, current, authority, stable, pending: None, failed: false })
    }

    /// Returns the independently provisioned deployment identity.
    pub fn domain(&self) -> &crate::log::catalog_head::CatalogDomain {
        self.authority.domain()
    }

    /// Returns the current stable head with its owned reader pin.
    pub fn view(&self) -> LazyCatalogView {
        self.stable.clone()
    }

    /// Returns the precise next update base, including internal staging/maintenance versions.
    pub fn write_root(&self) -> &Root {
        &self.current
    }

    /// Republishes an exact in-memory committed root after a certificate publication failure.
    /// Error cases:
    /// - a previous Store commit was uncertain, or durable head publication still fails
    pub fn retry_publication(&mut self) -> anyhow::Result<()> {
        ensure!(!self.failed, "catalog commit uncertain; rebuild derived namespace");
        if let Some(next) = &self.pending {
            self.authority.publish(next.certified().clone())?;
            self.stable = self.pending.take().expect("pending publication exists");
        }
        Ok(())
    }

    /// Runs official Store maintenance under the same serialized owner and republishes its root.
    /// The callback must verify exact reserved-key-only updates and finish Store file/directory sync.
    /// Error cases:
    /// - maintenance fails, modifies catalog metadata, regresses versions, or head publication fails
    pub fn maintenance(
        &mut self,
        operation: impl FnOnce(&Store, &Root) -> anyhow::Result<Root>,
    ) -> anyhow::Result<LazyCatalogView> {
        self.retry_publication()?;
        self.authority.prepare_maintenance()?;
        let before = self.current.clone();
        let after = match operation(&self.store, &before) {
            Ok(root) => root,
            Err(error) => {
                // A rollback leaves the store at `before`; only an uncertain commit ends this owner.
                self.failed = self.store.requires_reopen();
                return Err(error).context("catalog Store maintenance");
            }
        };
        if after == before {
            return Ok(self.stable.clone());
        }
        if after.namespace != before.namespace || after.version <= before.version {
            self.failed = true;
            anyhow::bail!("catalog maintenance root identity/version");
        }
        let next = root_with_meta(after.clone(), &self.stable.certified().meta());
        let view = match LazyCatalogView::open(self.read.clone(), next) {
            Ok(view) => view,
            Err(error) => {
                self.failed = true;
                return Err(error).context("maintenance changed catalog authority");
            }
        };
        self.current = after;
        self.pending = Some(view);
        self.retry_publication()?;
        Ok(self.stable.clone())
    }

    /// Aligns an existing live view with the same owner's published root, including GC-only versions.
    /// Error cases:
    /// - publication retry or exact live-prefix rebase fails
    pub(super) fn align(&mut self, view: &LazyCatalogView) -> anyhow::Result<LazyCatalogView> {
        self.retry_publication()?;
        view.rebase(self.stable.clone())
    }

    /// Commits one bounded stage without exposing its incomplete logical catalog to consumers.
    /// Error cases:
    /// - the exact Store update/durability operation fails; this owner cannot guess a retry root
    pub(super) fn stage(&mut self, changes: Vec<(Vec<u8>, Option<Vec<u8>>)>) -> anyhow::Result<()> {
        ensure!(!self.failed && self.pending.is_none(), "catalog owner is not writable");
        match self.store.update(&self.current, changes) {
            Ok(root) => {
                self.current = root;
                Ok(())
            }
            Err(error) => {
                // A rollback leaves the store at `current`, so a later stage retries from there.
                self.failed = self.store.requires_reopen();
                Err(error).context("catalog stage commit")
            }
        }
    }

    /// Makes the complete staged catalog visible only after root proof and signed durable tip ack.
    /// Error cases:
    /// - metadata/root cannot be verified or atomic publication fails
    pub(super) fn finish(&mut self, meta: CatalogMeta) -> anyhow::Result<LazyCatalogView> {
        let view = match LazyCatalogView::open(self.read.clone(), root_with_meta(self.current.clone(), &meta)) {
            Ok(view) => view,
            Err(error) => {
                self.failed = true;
                return Err(error).context("staged catalog metadata proof");
            }
        };
        self.pending = Some(view);
        self.retry_publication()?;
        Ok(self.stable.clone())
    }
}

/// Combines independently known logical metadata with the exact committed Store version.
fn root_with_meta(root: Root, meta: &CatalogMeta) -> CatalogRoot {
    CatalogRoot {
        root,
        completed_segments: meta.completed_segments,
        manifest_bytes: meta.manifest_bytes,
        next_seq: meta.next_seq,
        prefix: meta.prefix,
    }
}
