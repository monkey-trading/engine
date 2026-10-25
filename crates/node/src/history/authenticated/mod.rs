//! Account history at a complete authenticated command prefix; private staged roots are never heads.

mod state;
#[cfg(test)]
use state::CommandId;
pub use state::Head;
mod blobs;
mod ingest;
mod lists;
mod maintenance;
mod model;
mod overlay;
mod page;
mod staging;

use anyhow::Result;
use engine_core::types::Millis;

use crate::authenticated::{Lease, Store};
use crate::history::authenticated::state::checked_state;

/// The serialized storage owner's complete public view and its cancellation-safe version pin.
pub struct Index {
    store: Store,
    head: Head,
    _visible_pin: Lease,
    requires_rebuild: bool,
}

impl Index {
    /// Starts a fresh, parent-qualified namespace without deriving authority from disk.
    /// Error cases:
    /// - invalid retention, lease admission, or empty-root proof failure
    pub fn empty(store: Store, namespace: [u8; 32], retention_ms: Millis) -> Result<Self> {
        Self::resume(store, Head::empty(namespace, retention_ms)?)
    }

    /// Resumes only a parent-certified COMPLETE head, proving the fixed state leaf.
    /// Error cases:
    /// - unavailable/inconsistent state, a pending root, or failed proof/lease
    pub fn resume(store: Store, certified_head: Head) -> Result<Self> {
        let pin = store.lease(&certified_head.root)?;
        checked_state(&store, &certified_head)?;
        Ok(Self { store, head: certified_head, _visible_pin: pin, requires_rebuild: false })
    }

    /// Copies only the last complete, durable public head, including after a failed private stage.
    pub fn head(&self) -> Head {
        self.head.clone()
    }

    /// Reports that further writes require a fresh generation built from the verified source prefix.
    pub fn requires_rebuild(&self) -> bool {
        self.requires_rebuild
    }
}

#[cfg(test)]
mod tests;
