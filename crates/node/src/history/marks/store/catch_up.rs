//! A member's start from its replica leaves its marks cache empty until after its first command:
//! the cache its earlier holding left, then the marks its replica folded while it followed, are
//! taken by the marks task on a blocking thread (`run`), while its readers answer not ready.

use std::path::Path;
use std::sync::atomic::Ordering;

use anyhow::Context;
use rusqlite::TransactionBehavior;
use tracing::{info, warn};

use crate::history::marks::fold::MarkFold;
use crate::history::marks::store::{ChainAnchors, Store, discard_file, store};

/// How many folded buckets one merge transaction takes, so no one write holds the cache for long.
pub(crate) const MERGE_CHUNK: usize = 50_000;

impl Store {
    /// Has this store's readers answer not ready until `catch_up` has run.
    pub(crate) fn defer(&self) {
        self.ready.store(false, Ordering::Release);
    }

    /// Takes a member's earlier history into this cache, which holds nothing yet: the cache at
    /// `prior`, its earlier holding's, imported, validated (one that fails, host-altered, is cleared,
    /// as a startup rebuilds it) and then discarded, and the buckets its replica `folded` while it
    /// followed, merged `MERGE_CHUNK` at a time. Its readers are ready once it returns.
    /// Error cases:
    /// - the prior cache cannot be read or discarded, or a merge or the clear fails
    pub(crate) fn catch_up(&mut self, prior: Option<&Path>, folded: Option<MarkFold>) -> anyhow::Result<()> {
        if let Some(prior) = prior {
            let imported = self.import_prior(prior).context("importing the prior marks cache")?;
            if let Err(e) = self.validate_chains() {
                warn!(alert = "marks_cache_rebuilt", "the prior marks cache fails validation and is rebuilt: {e:#}");
                self.clear().context("clearing the invalid prior marks")?;
            }
            discard_file(prior).with_context(|| format!("discarding the prior marks cache {}", prior.display()))?;
            info!(imported, "imported the prior marks cache");
        }
        if let Some((buckets, newest)) = folded.and_then(MarkFold::into_buckets) {
            for chunk in buckets.chunks(MERGE_CHUNK) {
                self.observe_buckets(chunk, newest).context("merging the marks the replica folded while following")?;
            }
            info!(buckets = buckets.len(), "merged the marks the replica folded while following");
        }
        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    /// Deletes every row and backfill marker, and the anchors with them, keeping the run key the
    /// readers verify under.
    /// Error cases:
    /// - the delete fails
    fn clear(&mut self) -> anyhow::Result<()> {
        let tx =
            self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting a marks clear")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        tx.execute("DELETE FROM marks", []).context("clearing the marks")?;
        tx.execute("DELETE FROM backfilled", []).context("clearing the backfill markers")?;
        let mut anchors = self.anchors.lock().expect("the marks chain anchors are never poisoned");
        tx.commit().context("committing a marks clear")?;
        *anchors = ChainAnchors::default();
        Ok(())
    }
}
