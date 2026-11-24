//! A bounded checkpoint capture waits in the storage owner's capture slot until the owner takes it; the
//! owner pairs it with the history it has indexed at the capture's checkpoint, or sooner while the log
//! runs ahead of it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use engine_core::types::Millis;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tracing::info;

use crate::authenticated::CODEC;
use crate::config::SnapshotSection;
use crate::history::authenticated::Index;
#[cfg(test)]
use crate::history::runtime::Input;
use crate::log::CatalogBoundary;
use crate::log::catalog::LazyCatalogView;
use crate::snapshot::head::StoreId;
use crate::snapshot::lifecycle::Lifecycle;
use crate::snapshot::paired::{self, Pair, PrefixId};
use crate::snapshot::pins::Pins;
use crate::snapshot::{self, Snapshot, head};
use crate::stats::background::Encoded;
use crate::tee::EnclaveKey;

/// The longest the history owner waits for the stats worker's encoding of a capture's engine: far
/// more than an encode takes behind a full worker queue, so only a job never delivered reaches it.
pub(crate) const ENCODE_WAIT: Duration = Duration::from_secs(300);

/// How often a wait for an encoding looks at whether the history owner has been stopped.
const STOP_POLL: Duration = Duration::from_millis(100);

/// One optional in-flight snapshot bounds engine captures independently of writer lag: a capture has a
/// slot of its own beside the history feed's queue, so an index behind the log still takes it, and
/// completes it at the capture's checkpoint, or sooner while the log runs ahead of it
/// (`history::runtime`).
#[derive(Clone)]
pub(crate) struct Endpoint {
    /// The history owner's queue, which tests inspect through.
    #[cfg(test)]
    tx: mpsc::Sender<Input>,
    seals: mpsc::Sender<Box<Capture>>,
    permits: Arc<Semaphore>,
    pub write_alert: WriteAlert,
    /// The sequenced time between captures (`[snapshots] interval`), in milliseconds.
    pub interval_ms: Millis,
    /// Entries the owner has taken, read by tests without joining the owner's queue.
    #[cfg(test)]
    pub(crate) taken: Arc<std::sync::atomic::AtomicU64>,
    /// While set, the owner indexes none of the input it takes (`hold_index`).
    #[cfg(test)]
    pub(crate) index_held: Arc<AtomicBool>,
    /// How many calls currently wait for no capture to be in flight.
    #[cfg(test)]
    pub(crate) waiting: Arc<std::sync::atomic::AtomicUsize>,
    /// How many polls of the captures' encoding waits have ended without the encoding.
    #[cfg(test)]
    pub(crate) encode_polls: Arc<std::sync::atomic::AtomicUsize>,
    pub catalog_store: StoreId,
    /// The step of the next capture that fails, once, for tests.
    #[cfg(test)]
    fail: Arc<Mutex<Option<CaptureStage>>>,
}

/// A step of a capture after its slot is reserved, which a test can fail.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureStage {
    Sync,
    Seal,
    Fields,
    Encode,
}

/// Counts one live `idle` call for tests: `Drop` decrements exactly once however the wait ends, so
/// a timed-out or cancelled shutdown wait leaves no leaked waiter behind.
#[cfg(test)]
struct WaitCount(Arc<std::sync::atomic::AtomicUsize>);

#[cfg(test)]
impl WaitCount {
    fn enter(counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(counter)
    }
}

#[cfg(test)]
impl Drop for WaitCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Whether a `snapshot_write_failed` episode may be open. The sequencer and the history owner both
/// raise the alert, so each logs it, or its resolve, while holding this mark: a resolve can then
/// never follow a failure it did not see. It starts open, since an earlier run may have left an
/// episode open.
#[derive(Clone)]
pub(crate) struct WriteAlert(Arc<Mutex<bool>>);

/// All inputs were captured at the same physical checkpoint before that frontier was published.
pub(crate) struct Capture {
    /// The cache's signed fields but the engine, which `engine` gives.
    pub snapshot: Snapshot,
    pub engine: Encoding,
    pub ts: Millis,
    pub catalog_store: StoreId,
    pub catalog: CatalogBoundary,
    pub catalog_pin: Arc<LazyCatalogView>,
    pub _permit: OwnedSemaphorePermit,
    /// The endpoint's count of polls ended without an encoding.
    #[cfg(test)]
    pub encode_polls: Arc<std::sync::atomic::AtomicUsize>,
}

/// The capture slot of the history owner, held while a capture is prepared.
pub(crate) struct Slot<'a>(mpsc::Permit<'a, Box<Capture>>);

/// The engine at a capture's checkpoint: encoded on the sequencer's thread, or due from the stats
/// worker's copy (`stats::background::Job::Encode`).
pub(crate) enum Encoding {
    Encoded(String),
    Pending(Receiver<Encoded>),
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

    /// Connects the sequencer to the history owner's capture slot, `seals`, taking a capture every
    /// `interval` of sequenced time; one too long to count in milliseconds never comes.
    pub(crate) fn new(seals: mpsc::Sender<Box<Capture>>, catalog_store: StoreId, interval: Duration) -> Self {
        Self {
            #[cfg(test)]
            tx: mpsc::channel(1).0,
            seals,
            permits: Arc::new(Semaphore::new(1)),
            write_alert: WriteAlert(Arc::new(Mutex::new(true))),
            interval_ms: Millis::try_from(interval.as_millis()).unwrap_or(Millis::MAX),
            #[cfg(test)]
            taken: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(test)]
            index_held: Arc::default(),
            #[cfg(test)]
            waiting: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            encode_polls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            catalog_store,
            #[cfg(test)]
            fail: Arc::default(),
        }
    }

    /// Fails `stage` of the next capture, once.
    #[cfg(test)]
    pub(crate) fn fail_next(&self, stage: CaptureStage) {
        *self.fail.lock().unwrap_or_else(PoisonError::into_inner) = Some(stage);
    }

    /// Whether `stage` is the step to fail, which then works again.
    #[cfg(test)]
    pub(crate) fn take_fault(&self, stage: CaptureStage) -> bool {
        let mut fail = self.fail.lock().unwrap_or_else(PoisonError::into_inner);
        (*fail == Some(stage)).then(|| *fail = None).is_some()
    }

    /// This endpoint, inspecting the owner through its queue, `tx`.
    #[cfg(test)]
    pub(crate) fn inspecting(self, tx: mpsc::Sender<Input>) -> Self {
        Self { tx, ..self }
    }

    /// Holds the owner's index where it stands while `held`, the owner taking its input all the same;
    /// released, the owner indexes what it took at its next turn.
    #[cfg(test)]
    pub(crate) fn hold_index(&self, held: bool) {
        self.index_held.store(held, Ordering::Release);
    }

    /// Holds the owner where it takes this request, behind whatever its queue holds, until the returned
    /// sender sends or is dropped, so its input runs ahead of it meanwhile.
    #[cfg(test)]
    pub(crate) async fn pause(&self) -> Result<std::sync::mpsc::Sender<()>> {
        let (release, paused) = std::sync::mpsc::channel();
        self.tx.send(Input::Pause(paused)).await.map_err(|_| anyhow::anyhow!("history owner stopped"))?;
        Ok(release)
    }

    /// Whether the owner's queue has no free slot.
    #[cfg(test)]
    pub(crate) fn queue_full(&self) -> bool {
        self.tx.capacity() == 0
    }

    /// Whether the history owner has stopped taking input and captures.
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.tx.is_closed() && self.seals.is_closed()
    }

    /// Takes every free slot of the owner's queue until the returned permits drop, as an index behind
    /// the log leaves it.
    #[cfg(test)]
    pub(crate) fn hold_queue(&self) -> Vec<mpsc::OwnedPermit<Input>> {
        std::iter::from_fn(|| self.tx.clone().try_reserve_owned().ok()).collect()
    }

    /// A busy storage owner skips an optional capture without delaying public event publication.
    pub(crate) fn try_permit(&self) -> Option<OwnedSemaphorePermit> {
        self.permits.clone().try_acquire_owned().ok()
    }

    /// Resolves once no capture is in flight, so the shutdown checkpoint can be captured too.
    pub(crate) async fn idle(&self) {
        // The count drops however the wait ends, a timeout included, so `waiting` never leaks.
        #[cfg(test)]
        let _waiting = WaitCount::enter(self.waiting.clone());
        // The semaphore is never closed, so acquiring only waits.
        let _ = self.permits.acquire().await;
    }

    /// Reserves the owner's capture slot before a capture's work is done, so a capture it would refuse
    /// costs nothing, and one prepared is always taken.
    /// Error cases:
    /// - the supervised owner has stopped, or its slot holds a capture it has not taken, which the one
    ///   capture permit (`try_permit`) taken first rules out
    pub(crate) fn reserve(&self) -> Result<Slot<'_>> {
        self.seals.try_reserve().map(Slot).map_err(|_| anyhow::anyhow!("the history owner takes no capture"))
    }
}

impl Slot<'_> {
    /// Hands the capture over before the checkpoint's flush frontier or events are published, so the
    /// owner takes it before it takes the checkpoint.
    pub(crate) fn submit(self, capture: Capture) {
        self.0.send(Box::new(capture));
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

impl Encoding {
    /// The encoding, waiting up to `within` for the stats worker's when it is due from there, and no
    /// longer once `stopped` answers true, which it is asked at the end of each poll of `STOP_POLL`, so a
    /// stopped history owner is not held by it. A worker that stopped drops the job and its reply; a job a
    /// stopping sequencer never delivered fails the wait, so the history owner is never held past it.
    /// Error cases:
    /// - the worker stopped, its copy is not at the checkpoint or did not encode, no encoding came within
    ///   `within`, or `stopped` answered true first
    pub(crate) fn wait(self, within: Duration, mut stopped: impl FnMut() -> bool) -> Result<String> {
        let reply = match self {
            Self::Encoded(engine) => return Ok(engine),
            Self::Pending(reply) => reply,
        };
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match reply.recv_timeout(left.min(STOP_POLL)) {
                Ok(encoded) => {
                    return encoded.map_err(|reason| anyhow::anyhow!("the stats worker encoded no engine: {reason}"));
                }
                Err(RecvTimeoutError::Timeout) if stopped() => {
                    anyhow::bail!("the history owner stopped before the engine was encoded")
                }
                Err(RecvTimeoutError::Timeout) if left > STOP_POLL => {}
                Err(RecvTimeoutError::Timeout) => anyhow::bail!("the stats worker encoded no engine within {within:?}"),
                Err(RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("the stats worker stopped before encoding the engine")
                }
            }
        }
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

    /// Completes history authentication and durable paired publication on the blocking owner, pairing the
    /// capture with the history `history` has indexed, at or before its checkpoint.
    /// Error cases:
    /// - any acknowledgement differs, the history has passed the checkpoint, required bytes/proofs are
    ///   missing, or publication fails
    pub(crate) fn complete(
        mut self,
        cfg: &SnapshotSection,
        history: &mut HistorySeal<'_>,
        (key, lifecycle): (&EnclaveKey, &Lifecycle),
        stopped: &AtomicBool,
    ) -> Result<()> {
        let prefix = self.prefix();
        ensure!(
            self.catalog.checkpoint.offset == prefix.offset,
            "catalog acknowledged a different physical checkpoint"
        );
        ensure!(self.catalog.checkpoint.checkpoint == prefix.checkpoint, "catalog acknowledged a different checkpoint");
        let head = history.index.head();
        ensure!(
            head.last_seq().is_some_and(|seq| seq <= prefix.offset.seq) && head.last_ts() <= Some(prefix.ts),
            "history has indexed nothing, or past the captured checkpoint"
        );
        if head.last_seq() == Some(prefix.offset.seq) {
            ensure!(head.last_ts() == Some(prefix.ts), "history acknowledged a different command frontier");
            ensure!(
                head.command_prefix == prefix.command_prefix,
                "history acknowledged a different command/event prefix"
            );
        }
        let history_seq = head.last_seq();
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
            format: paired::FORMAT,
            codec: CODEC,
            prefix: prefix.clone(),
            history: staged_head.certificate(key)?,
            catalog_store: self.catalog_store,
            catalog: self.catalog,
        };
        pair.verify(cfg, &prefix, history.index.head().retention_ms)?;
        self.snapshot.schema = 3;
        self.snapshot.pair = Some(pair);
        // Last before staging, so the worker encodes while the history head is proved and signed.
        let polled = || {
            #[cfg(test)]
            self.encode_polls.fetch_add(1, Ordering::AcqRel);
            stopped.load(Ordering::Acquire)
        };
        self.snapshot.engine = self.engine.wait(ENCODE_WAIT, polled)?;
        let staged = snapshot::stage(cfg, &self.snapshot, key)?;
        let payloads = lifecycle.payloads();
        let mut publication = lifecycle.publication()?;
        publication.settle()?;
        staged_head.publish(&mut publication)?;
        // Pruning rescans the family; the payload just signed needs no read back under the lock.
        staged.publish_then(&mut publication, |path| payloads.seed(path, self.snapshot))?;
        info!(seq = prefix.offset.seq, history_seq = ?history_seq, "checkpoint snapshot stored");
        Ok(())
    }
}

#[cfg(test)]
mod tests;
