//! Read-only proof access and parent-supplied current-version inspection.

use std::sync::Arc;

use anyhow::Result;

use crate::authenticated::{Root, Store};

pub type CatalogPin = Arc<dyn Send + Sync>;

/// Preflight implementations must open no writable files, recover no journal, and create no lock.
pub trait CatalogRead: Send + Sync {
    /// Verifies membership/nonmembership against this exact caller-authenticated root.
    /// Error cases:
    /// - IO, decoding, resource bounds, or upstream JMT proof verification fails
    fn read(&self, root: &Root, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Holds an owned root lease through the actual blocking read and its cancellation.
    /// Error cases:
    /// - the version cannot be pinned
    fn pin(&self, root: &Root) -> Result<CatalogPin>;

    /// Refuses a stale/unacknowledged write base using bounded read-only storage inspection.
    /// SQL observations can refuse a cache; they never nominate a trusted replacement root.
    /// Error cases:
    /// - an interrupted commit, hot journal, newer version, or unavailable root is detected
    fn ensure_current(&self, root: &Root) -> Result<()>;
}

/// Checks that no unacknowledged version follows a root; supplied by the storage owner.
pub type CurrentProbe = Arc<dyn Fn(&Root) -> Result<()> + Send + Sync>;

/// Writable Store access belongs after preflight; current_probe is supplied by the storage owner.
pub struct StoreCatalogRead {
    store: Arc<Store>,
    current_probe: CurrentProbe,
}

impl StoreCatalogRead {
    /// Wraps existing ownership; constructing the read-only preflight adapter is the parent's job.
    pub fn new(store: Arc<Store>, current_probe: CurrentProbe) -> Self {
        Self { store, current_probe }
    }
}

impl CatalogRead for StoreCatalogRead {
    /// Delegates every result to the official proof-verifying Store adapter.
    /// Error cases:
    /// - Store rejects the exact root or proof
    fn read(&self, root: &Root, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.store.read(root, key)
    }

    /// Retains the Store's concrete owned lease without exposing its representation.
    /// Error cases:
    /// - Store cannot pin the root
    fn pin(&self, root: &Root) -> Result<CatalogPin> {
        Ok(Arc::new(self.store.lease(root)?))
    }

    /// Invokes the parent's bounded read-only collision/fault guard.
    /// Error cases:
    /// - the guard detects an unsuitable write base
    fn ensure_current(&self, root: &Root) -> Result<()> {
        (self.current_probe)(root)
    }
}
