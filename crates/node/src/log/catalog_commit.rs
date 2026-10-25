//! Bounded staging of arbitrarily many segment locators, followed by one visible catalog commit.

use anyhow::{Context, ensure};

use crate::authenticated::{MAX_BATCH_BYTES, MAX_UPDATES};
use crate::log::catalog::{
    CatalogMeta, CheckpointLocator, LazyCatalogView, META, checkpoint_key, ordinal_key, validate_locator,
};
use crate::log::catalog_owner::CatalogOwner;

struct Staging<'a> {
    owner: &'a mut CatalogOwner,
    changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    bytes: usize,
    changed: bool,
}

/// Folds exact live completions and active locators using the current certified writer base only.
/// Error cases:
/// - any old-value proof, bounded stage, metadata commit, or durable tip publication fails
pub(super) fn fold(
    view: &LazyCatalogView,
    owner: &mut CatalogOwner,
    active: &[CheckpointLocator],
) -> anyhow::Result<LazyCatalogView> {
    let view = owner.align(view)?;
    let meta = CatalogMeta {
        completed_segments: view.len(),
        manifest_bytes: view.manifest_bytes(),
        next_seq: view.next_seq(),
        prefix: view.prefix(),
    };
    let changed = {
        let mut stage = Staging { owner, changes: Vec::new(), bytes: 0, changed: false };
        for completion in view.delta.iter() {
            for locator in completion.checkpoints.iter() {
                stage.locator(locator)?;
            }
            stage.exact(ordinal_key(completion.dense.ordinal), serde_json::to_vec(&completion.dense)?, true)?;
        }
        for locator in active {
            stage.locator(locator)?;
        }
        // Every earlier stage retains the old visible count/prefix. This is the last logical change.
        stage.exact(META.to_vec(), serde_json::to_vec(&meta)?, false)?;
        stage.flush()?;
        stage.changed
    };
    if !changed {
        return Ok(view);
    }
    owner.finish(meta)
}

impl Staging<'_> {
    /// Requires an exact old locator or inserts it; conflicting historical framing never changes.
    /// Error cases:
    /// - locator identity, membership proof, or exact value consistency fails
    fn locator(&mut self, locator: &CheckpointLocator) -> anyhow::Result<()> {
        validate_locator(locator, locator.offset)?;
        self.exact(checkpoint_key(locator.offset), serde_json::to_vec(locator)?, false)
    }

    /// Adds one exact value, flushing only at the Store's declared update/byte envelopes. A value
    /// already stored byte for byte, which a retried fold's earlier committed stage leaves, is kept.
    /// Error cases:
    /// - an existing immutable value conflicts or a single value exceeds the Store envelope
    fn exact(&mut self, key: Vec<u8>, value: Vec<u8>, absent: bool) -> anyhow::Result<()> {
        if let Some((_, old)) = self.changes.iter().find(|(existing, _)| existing == &key) {
            ensure!(old.as_deref() == Some(value.as_slice()) && !absent, "duplicate/conflicting staged key");
            return Ok(());
        }
        let old = self.owner.read.read(&self.owner.current, &key)?;
        if old.as_deref() == Some(value.as_slice()) {
            return Ok(());
        }
        if absent {
            ensure!(old.is_none(), "catalog ordinal already occupied");
        }
        if key.as_slice() != META {
            ensure!(old.is_none(), "checkpoint locator changed");
        }
        let bytes = key.len().checked_add(value.len()).context("catalog stage size overflow")?;
        ensure!(bytes <= MAX_BATCH_BYTES, "catalog value exceeds Store batch envelope");
        if self.changes.len() == MAX_UPDATES || bytes > MAX_BATCH_BYTES - self.bytes {
            self.flush()?;
        }
        self.bytes += bytes;
        self.changes.push((key, Some(value)));
        Ok(())
    }

    /// Durably stores one internal stage; only finish() may publish the new completed catalog.
    /// Error cases:
    /// - exact update proof, immutable-version storage, or file/directory sync fails
    fn flush(&mut self) -> anyhow::Result<()> {
        if self.changes.is_empty() {
            return Ok(());
        }
        let mut changes = std::mem::take(&mut self.changes);
        changes.sort_by(|a, b| a.0.cmp(&b.0));
        self.owner.stage(changes)?;
        self.changed = true;
        self.bytes = 0;
        Ok(())
    }
}
