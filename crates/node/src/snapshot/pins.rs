//! Cache-version pins are restored on the same store owners before writes or garbage collection.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;

use crate::authenticated::{Lease, Root, Store};
use crate::config::SnapshotSection;
use crate::log::CheckpointDomain;
use crate::snapshot::head::StoreId;
use crate::snapshot::payloads::{Cached, Payloads};
use crate::snapshot::{self, head};

/// Returns authenticated C root identities; unsigned filenames never retain an attempted root.
/// Error cases:
/// - metadata enumeration or IO is incomplete, so the caller must retain its old version leases
pub(crate) fn catalog_references(
    cfg: &SnapshotSection,
    domain: CheckpointDomain,
    payloads: &Payloads,
) -> Result<Vec<(crate::log::Offset, Root)>> {
    let mut roots = Vec::new();
    // A rejected cache holds no root; the inventory scan reports it.
    for (_, verified) in payloads.verified(cfg, domain.chain_id, domain.vault)? {
        if let Cached::Payload(snapshot) = verified?
            && let Some(pair) = &snapshot.pair
        {
            roots.push((snapshot.offset, pair.catalog.catalog.root.clone()));
        }
    }
    Ok(roots)
}

/// One database owner's retained cache roots, including uncertain publication attempts.
pub(crate) struct Pins {
    id: StoreId,
    store: Store,
    payloads: Arc<Payloads>,
    held: BTreeMap<Root, Lease>,
}

impl Pins {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.held.len()
    }

    /// Creates an empty pin set without discovering roots from unsigned database rows.
    pub(crate) fn new(id: StoreId, store: Store, payloads: Arc<Payloads>) -> Self {
        Self { id, store, payloads, held: BTreeMap::new() }
    }

    /// Keeps an attempted publication alive even if rename succeeded but directory sync failed.
    /// Error cases:
    /// - leasing a retired, invalid or unavailable root fails
    pub(crate) fn protect(&mut self, root: &Root) -> Result<()> {
        if !self.held.contains_key(root) {
            self.held.insert(root.clone(), self.store.lease(root)?);
        }
        Ok(())
    }

    /// Replaces pins only after a complete read of every retained signed metadata reference.
    /// Error cases:
    /// - listing, certificate verification or required-version leasing fails
    pub(crate) fn refresh(&mut self, cfg: &SnapshotSection, domain: CheckpointDomain) -> Result<()> {
        let mut next = BTreeMap::new();
        for (_, verified) in self.payloads.verified(cfg, domain.chain_id, domain.vault)? {
            // A rejected cache holds no root; the inventory scan reports it.
            let Cached::Payload(snapshot) = verified? else { continue };
            if let Some(pair) = &snapshot.pair {
                let history = head::verify_certificate(cfg, &pair.history, domain)?;
                for (id, root) in
                    [(history.store, history.head.root), (pair.catalog_store, pair.catalog.catalog.root.clone())]
                {
                    if id == self.id {
                        let lease = self.store.lease(&root)?;
                        next.insert(root, lease);
                    }
                }
            }
        }
        for path in head::candidates_readonly(cfg)? {
            let statement = match head::read(cfg, &path, domain) {
                Ok(statement) => statement,
                Err(error) => {
                    rejected_reference(&path, error)?;
                    continue;
                }
            };
            if statement.store == self.id {
                let root = statement.head.root;
                let lease = self.store.lease(&root)?;
                next.insert(root, lease);
            }
        }
        for root in snapshot::catalog_heads::retained_roots(cfg, self.id, domain)? {
            let lease = self.store.lease(&root)?;
            next.insert(root, lease);
        }
        self.held = next;
        Ok(())
    }
}

/// Whether an error came from IO, which bookkeeping must not treat as a rejected cache.
pub(super) fn is_io(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.downcast_ref::<std::io::Error>().is_some())
}

/// Invalid optional caches cannot authorize a root; unreadable metadata defers all retirement.
/// Error cases:
/// - metadata IO is incomplete, so the previous retained-reference set must remain pinned
pub(super) fn rejected_reference(path: &std::path::Path, error: anyhow::Error) -> Result<()> {
    if is_io(&error) {
        return Err(error);
    }
    snapshot::rejected(path, &error);
    Ok(())
}
