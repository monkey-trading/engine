//! A bounded checkpoint capture waits for the exact history fence on the storage owner's queue.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Result, ensure};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tracing::info;

use crate::authenticated::CODEC;
use crate::config::SnapshotSection;
use crate::history::authenticated::Index;
use crate::history::runtime::Input;
use crate::log::CatalogBoundary;
use crate::log::catalog::LazyCatalogView;
use crate::snapshot::head::StoreId;
use crate::snapshot::lifecycle::Lifecycle;
use crate::snapshot::paired::{Pair, PrefixId};
use crate::snapshot::pins::Pins;
use crate::snapshot::{self, Snapshot, head};
use crate::tee::EnclaveKey;

/// One optional in-flight snapshot bounds engine captures independently of writer lag.
#[derive(Clone)]
pub(crate) struct Endpoint {
    tx: mpsc::Sender<Input>,
    permits: Arc<Semaphore>,
    pub write_alert: WriteAlert,
    /// Entries the owner has taken, read by tests without joining the owner's queue.
    #[cfg(test)]
    pub(crate) taken: Arc<std::sync::atomic::AtomicU64>,
    /// How many calls currently wait for no capture to be in flight.
    #[cfg(test)]
    pub(crate) waiting: Arc<std::sync::atomic::AtomicUsize>,
    pub catalog_store: StoreId,
}

/// Whether a `snapshot_write_failed` episode may be open. The sequencer and the history owner both
/// raise the alert, so each logs it, or its resolve, while holding this mark: a resolve can then
/// never follow a failure it did not see. It starts open, since an earlier run may have left an
/// episode open.
#[derive(Clone)]
pub(crate) struct WriteAlert(Arc<Mutex<bool>>);

/// All inputs were captured at the same physical checkpoint before that frontier was published.
pub(crate) struct Capture {
    pub snapshot: Snapshot,
    pub ts: engine_core::types::Millis,
    pub catalog_store: StoreId,
    pub catalog: CatalogBoundary,
    pub catalog_pin: Arc<LazyCatalogView>,
    pub _permit: OwnedSemaphorePermit,
}

pub(crate) struct HistorySeal<'a> {
    pub id: StoreId,
    pub store: &'a crate::authenticated::Store,
    pub index: &'a Index,
    pub pins: &'a mut Pins,
}

impl Endpoint {
    /// Samples only after the blocking owner reaches this queue message; no polling sleep.
    #[cfg(test)]
    pub(crate) async fn inspect(&self) -> Result<crate::history::runtime::TestState> {
        let (done, received) = tokio::sync::oneshot::channel();
        self.tx.send(Input::Inspect(done)).await.map_err(|_| anyhow::anyhow!("history owner stopped"))?;
        received.await.map_err(|_| anyhow::anyhow!("history owner stopped before inspection"))
    }

    /// Connects the sequencer directly to the same bounded queue used by the history feed.
    pub(crate) fn new(tx: mpsc::Sender<Input>, catalog_store: StoreId) -> Self {
        Self {
            tx,
            permits: Arc::new(Semaphore::new(1)),
            write_alert: WriteAlert(Arc::new(Mutex::new(true))),
            #[cfg(test)]
            taken: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(test)]
            waiting: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            catalog_store,
        }
    }

    /// Whether the history owner has stopped taking input.
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// A busy storage owner skips an optional capture without delaying public event publication.
    pub(crate) fn try_permit(&self) -> Option<OwnedSemaphorePermit> {
        self.permits.clone().try_acquire_owned().ok()
    }

    /// Resolves once no capture is in flight, so the shutdown checkpoint can be captured too.
    pub(crate) async fn idle(&self) {
        #[cfg(test)]
        self.waiting.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The semaphore is never closed, so acquiring only waits.
        let _ = self.permits.acquire().await;
        #[cfg(test)]
        self.waiting.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Queues control before the checkpoint's flush frontier or events are published.
    /// Error cases:
    /// - the optional storage queue is full or its supervised owner has stopped
    pub(crate) fn submit(&self, capture: Capture) -> Result<()> {
        self.tx
            .try_send(Input::Seal(Box::new(capture)))
            .map_err(|_| anyhow::anyhow!("optional snapshot queue unavailable"))
    }
}

impl WriteAlert {
    /// Marks the episode open while `raise` logs its `snapshot_write_failed` Warning.
    pub(crate) fn failed(&self, raise: impl FnOnce()) {
        let mut open = self.lock();
        *open = true;
        raise();
    }

    /// Takes the mark as a capture starts completing; `stored` resolves only what it took.
    pub(crate) fn take(&self) -> bool {
        std::mem::take(&mut *self.lock())
    }

    /// Resolves the episode that `take` saw open, now that its capture is stored, unless a failure
    /// was raised since: that one stays open for the next stored capture.
    pub(crate) fn stored(&self, taken: bool) {
        let open = self.lock();
        if taken && !*open {
            info!(alert_resolved = "snapshot_write_failed", "a checkpoint snapshot is stored");
        }
    }

    /// The mark is a plain flag, so a panic while logging leaves nothing half-updated.
    fn lock(&self) -> MutexGuard<'_, bool> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Capture {
    /// Names the engine's exact physical checkpoint and complete command/event prefix.
    pub(crate) fn prefix(&self) -> PrefixId {
        PrefixId {
            domain: crate::log::CheckpointDomain { chain_id: self.snapshot.chain_id, vault: self.snapshot.vault },
            offset: self.snapshot.offset,
            checkpoint: self.snapshot.checkpoint.clone(),
            command_prefix: self.snapshot.context.command_prefix,
            ts: self.ts,
        }
    }

    /// Completes history authentication and durable paired publication on the blocking owner.
    /// Error cases:
    /// - any acknowledgement differs, required bytes/proofs are missing, or publication fails
    pub(crate) fn complete(
        mut self,
        cfg: &SnapshotSection,
        history: &mut HistorySeal<'_>,
        key: &EnclaveKey,
        lifecycle: &Lifecycle,
    ) -> Result<()> {
        let prefix = self.prefix();
        ensure!(
            self.catalog.checkpoint.offset == prefix.offset,
            "catalog acknowledged a different physical checkpoint"
        );
        ensure!(self.catalog.checkpoint.checkpoint == prefix.checkpoint, "catalog acknowledged a different checkpoint");
        let head = history.index.head();
        ensure!(
            head.last_seq() == Some(prefix.offset.seq) && head.last_ts() == Some(prefix.ts),
            "history acknowledged a different command frontier"
        );
        ensure!(head.command_prefix == prefix.command_prefix, "history acknowledged a different command/event prefix");
        self.snapshot.context.validate(&prefix.checkpoint, prefix.domain)?;
        {
            let mut publication = lifecycle.publication()?;
            publication.settle()?;
            history.pins.refresh(cfg, prefix.domain)?;
            history.pins.protect(&head.root)?;
            head::prepare(cfg, &mut publication)?;
            snapshot::prepare(cfg, &mut publication)?;
        }
        // Proving, encoding, signing and syncing run outside the lifecycle lock, which the
        // sequencer takes to publish catalog heads; the lock covers only the renames and pruning.
        let staged_head = head::stage(cfg, prefix.domain, history.id, history.store, head, key)?;
        let pair = Pair {
            format: 1,
            codec: CODEC,
            prefix: prefix.clone(),
            history: staged_head.certificate(key)?,
            catalog_store: self.catalog_store,
            catalog: self.catalog,
        };
        pair.verify(cfg, &prefix, history.index.head().retention_ms)?;
        self.snapshot.schema = 3;
        self.snapshot.pair = Some(pair);
        let staged = snapshot::stage(cfg, &self.snapshot, key)?;
        let payloads = lifecycle.payloads();
        let mut publication = lifecycle.publication()?;
        publication.settle()?;
        staged_head.publish(&mut publication)?;
        // Pruning rescans the family; the payload just signed needs no read back under the lock.
        staged.publish_then(&mut publication, |path| payloads.seed(path, self.snapshot))?;
        info!(seq = prefix.offset.seq, "checkpoint snapshot stored");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tracing::warn;

    use super::WriteAlert;
    use crate::alerts::capture;

    /// A stored capture resolves the episode open when it started completing, but not a failure
    /// raised while it completed, such as the shutdown wait's: that one stays open for the next.
    #[test]
    fn a_stored_capture_resolves_only_failures_raised_before_its_take() {
        let alert = WriteAlert(std::sync::Arc::new(std::sync::Mutex::new(true)));
        let lines = capture(|| {
            let taken = alert.take();
            alert.failed(|| warn!(alert = "snapshot_write_failed", "raised while a capture completes"));
            alert.stored(taken);
            let taken = alert.take();
            alert.stored(taken);
            let taken = alert.take();
            alert.stored(taken);
        });
        let firing: Vec<_> = lines.iter().map(|line| line.severity.is_some()).collect();
        assert_eq!(firing, [true, false], "one failure, then one resolve by the next stored capture: {lines:#?}");
    }
}
