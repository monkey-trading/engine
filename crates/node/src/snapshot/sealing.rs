//! A bounded checkpoint capture waits for the exact history fence on the storage owner's queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// Whether a `snapshot_write_failed` episode may be open, shared with the history owner, which
    /// clears it and resolves the alert when it next stores a capture. It starts set, since an
    /// earlier run may have left an episode open.
    write_alert_open: Arc<AtomicBool>,
    pub catalog_store: StoreId,
}

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
            write_alert_open: Arc::new(AtomicBool::new(true)),
            catalog_store,
        }
    }

    /// The flag the history owner clears when it stores a capture.
    pub(crate) fn write_alert_open(&self) -> Arc<AtomicBool> {
        self.write_alert_open.clone()
    }

    /// Records that the sequencer raised `snapshot_write_failed` for a capture it could not hand
    /// over or wait for, so the next capture the history owner stores resolves it.
    pub(crate) fn write_failed(&self) {
        self.write_alert_open.store(true, Ordering::Release);
    }

    /// A busy storage owner skips an optional capture without delaying public event publication.
    pub(crate) fn try_permit(&self) -> Option<OwnedSemaphorePermit> {
        self.permits.clone().try_acquire_owned().ok()
    }

    /// Resolves once no capture is in flight, so the shutdown checkpoint can be captured too.
    pub(crate) async fn idle(&self) {
        // The semaphore is never closed, so acquiring only waits.
        let _ = self.permits.acquire().await;
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
