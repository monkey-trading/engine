//! A bounded checkpoint capture waits for the exact history fence on the storage owner's queue.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Result, ensure};
use engine_core::types::Millis;
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
use crate::stats::background::Encoded;
use crate::tee::EnclaveKey;

/// The longest the history owner waits for the stats worker's encoding of a capture's engine: far
/// more than an encode takes behind a full worker queue, so only a job never delivered reaches it.
const ENCODE_WAIT: Duration = Duration::from_secs(300);

/// One optional in-flight snapshot bounds engine captures independently of writer lag.
#[derive(Clone)]
pub(crate) struct Endpoint {
    tx: mpsc::Sender<Input>,
    permits: Arc<Semaphore>,
    pub write_alert: WriteAlert,
    /// The sequenced time between captures (`[snapshots] interval`), in milliseconds.
    pub interval_ms: Millis,
    /// Entries the owner has taken, read by tests without joining the owner's queue.
    #[cfg(test)]
    pub(crate) taken: Arc<std::sync::atomic::AtomicU64>,
    /// How many calls currently wait for no capture to be in flight.
    #[cfg(test)]
    pub(crate) waiting: Arc<std::sync::atomic::AtomicUsize>,
    pub catalog_store: StoreId,
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
}

/// A slot in the history owner's queue, held while a capture is prepared.
pub(crate) struct Slot<'a>(mpsc::Permit<'a, Input>);

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

    /// Connects the sequencer directly to the same bounded queue used by the history feed, taking a
    /// capture every `interval` of sequenced time; one too long to count in milliseconds never comes.
    pub(crate) fn new(tx: mpsc::Sender<Input>, catalog_store: StoreId, interval: Duration) -> Self {
        Self {
            tx,
            permits: Arc::new(Semaphore::new(1)),
            write_alert: WriteAlert(Arc::new(Mutex::new(true))),
            interval_ms: Millis::try_from(interval.as_millis()).unwrap_or(Millis::MAX),
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
        // The count drops however the wait ends, a timeout included, so `waiting` never leaks.
        #[cfg(test)]
        let _waiting = WaitCount::enter(self.waiting.clone());
        // The semaphore is never closed, so acquiring only waits.
        let _ = self.permits.acquire().await;
    }

    /// Reserves the owner's queue slot for a capture before its work is done, so a capture the queue
    /// would refuse costs nothing, and one prepared is always taken.
    /// Error cases:
    /// - the optional storage queue is full or its supervised owner has stopped
    pub(crate) fn reserve(&self) -> Result<Slot<'_>> {
        self.tx.try_reserve().map(Slot).map_err(|_| anyhow::anyhow!("optional snapshot queue unavailable"))
    }
}

impl Slot<'_> {
    /// Queues control before the checkpoint's flush frontier or events are published.
    pub(crate) fn submit(self, capture: Capture) {
        self.0.send(Input::Seal(Box::new(capture)));
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
    /// The encoding, waiting up to `within` for the stats worker's when it is due from there. A worker
    /// that stopped drops the job and its reply; a job a stopping sequencer never delivered fails the
    /// wait, so the history owner is never held past it.
    /// Error cases:
    /// - the worker stopped, its copy is not at the checkpoint or did not encode, or no encoding came
    ///   within `within`
    fn wait(self, within: Duration) -> Result<String> {
        match self {
            Self::Encoded(engine) => Ok(engine),
            Self::Pending(reply) => match reply.recv_timeout(within) {
                Ok(encoded) => {
                    encoded.map_err(|reason| anyhow::anyhow!("the stats worker encoded no engine: {reason}"))
                }
                Err(RecvTimeoutError::Timeout) => anyhow::bail!("the stats worker encoded no engine within {within:?}"),
                Err(RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("the stats worker stopped before encoding the engine")
                }
            },
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
        // Last before staging, so the worker encodes while the history head is proved and signed.
        self.snapshot.engine = self.engine.wait(ENCODE_WAIT)?;
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
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use tracing::warn;

    use super::{Encoding, Endpoint, WriteAlert};
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

    /// An engine due from the stats worker is taken when it comes, and a wait fails, never hangs, when
    /// the worker stops without it, refuses it, or it never comes.
    #[test]
    fn a_pending_encoding_is_taken_when_it_comes_and_fails_when_it_does_not() {
        let within = Duration::from_millis(50);
        let pending = || std::sync::mpsc::sync_channel::<crate::stats::background::Encoded>(1);
        assert_eq!(Encoding::Encoded("{}".into()).wait(within).unwrap(), "{}");
        let (reply, encoded) = pending();
        reply.send(Ok("{\"engine\":1}".into())).unwrap();
        assert_eq!(Encoding::Pending(encoded).wait(within).unwrap(), "{\"engine\":1}");
        let (reply, encoded) = pending();
        reply.send(Err("the copy is at None".into())).unwrap();
        let refused = Encoding::Pending(encoded).wait(within).unwrap_err();
        assert!(format!("{refused:#}").contains("the copy is at None"), "{refused:#}");
        let (reply, encoded) = pending();
        drop(reply);
        let stopped = Encoding::Pending(encoded).wait(within).unwrap_err();
        assert!(format!("{stopped:#}").contains("stopped before encoding"), "{stopped:#}");
        // A job never delivered: the sender lives on, unsent.
        let (_reply, encoded) = pending();
        let never = Encoding::Pending(encoded).wait(within).unwrap_err();
        assert!(format!("{never:#}").contains("within"), "{never:#}");
    }

    /// A shutdown capture wait ends by cancellation when it times out, or by abort on task drop;
    /// either way the test-only `waiting` count returns to zero, so later assertions never see a
    /// leaked waiter after the shutdown path ran.
    #[tokio::test]
    async fn a_dropped_idle_wait_releases_its_waiter_count() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let endpoint = Endpoint::new(tx, crate::snapshot::head::StoreId([0x42; 32]), Duration::ZERO);
        let in_flight = endpoint.try_permit().expect("no capture is in flight");
        let deadline = Instant::now() + Duration::from_secs(5);

        let waiting = tokio::spawn({
            let endpoint = endpoint.clone();
            async move { endpoint.idle().await }
        });
        while endpoint.waiting.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "idle never counts its waiter");
            tokio::task::yield_now().await;
        }
        // Cancellation — how a shutdown wait ends on timeout — drops the acquire future, so the
        // count must fall before the cancelled task's join resolves.
        waiting.abort();
        let _ = waiting.await;
        assert_eq!(endpoint.waiting.load(Ordering::Acquire), 0, "a cancelled idle wait leaks its waiter");

        // The shutdown path wraps the wait in a timeout and moves on; the dropped future must
        // release the count just the same.
        tokio::time::timeout(Duration::from_millis(10), endpoint.idle()).await.expect_err("the permit stays held");
        assert_eq!(endpoint.waiting.load(Ordering::Acquire), 0, "a timed-out idle wait leaks its waiter");

        // The resolving wait still decrements exactly once.
        drop(in_flight);
        endpoint.idle().await;
        assert_eq!(endpoint.waiting.load(Ordering::Acquire), 0);
    }
}
