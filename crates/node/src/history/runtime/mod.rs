//! One supervised blocking owner serializes history, checkpoint sealing and verified repair. A capture
//! is paired with the history indexed at its checkpoint, or, while the owner's input runs ahead of it,
//! with the history indexed by the end of the turn that took it, so it never waits for a backlog.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail, ensure};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::spawn_blocking;
use tracing::{info, warn};

use crate::api::Feed;
use crate::authenticated::Store;
use crate::config::SnapshotSection;
use crate::history::authenticated::{Head, Index};
use crate::history::derived::{Deriver, Taken};
use crate::history::reader::{Generation, Published, Reader};
use crate::log::{CheckpointDomain, Offset};
use crate::sequencer::{StreamBody, StreamMsg};
use crate::snapshot::head::StoreId;
use crate::snapshot::lifecycle::Lifecycle;
use crate::snapshot::{
    pins::Pins,
    sealing::{Capture, Endpoint, WriteAlert},
};
use crate::tee::EnclaveKey;

mod capture;
mod heads;
mod stop;

/// Commands indexed between signed heads outside captured checkpoints and shutdown, and between
/// collections of retired versions. Both cost several fsyncs, so they run at this cadence rather
/// than on every turn; a crash re-indexes at most this many commands from the log.
const CERTIFY_COMMANDS: u64 = 1024;

/// Collection calls per history turn. Each call consumes up to `MAX_UPDATES` whole receipts; a
/// collection still progressing after this many continues on the next turn. Tests use two, so a
/// small backlog spans turns.
const MAX_COLLECTION_TURNS: usize = if cfg!(test) { 2 } else { 32 };

/// Commands per history update. Each update ends in one synced commit, so an owner behind the log
/// catches up faster in larger ones; a requested stop waits for at most one update, so this bounds
/// how long shutdown waits for history once its grace has run out.
pub(crate) const INDEX_BATCH: usize = 512;

pub(crate) enum Input {
    Entry(Arc<StreamMsg>),
    Repair,
    Drain(oneshot::Sender<()>),
    Stop,
    #[cfg(test)]
    Inspect(oneshot::Sender<TestState>),
    /// Holds the owner where it takes this input until the sender sends or is dropped, for tests that
    /// let its input run ahead of it.
    #[cfg(test)]
    Pause(std::sync::mpsc::Receiver<()>),
}

#[cfg(test)]
pub(crate) struct TestState {
    pub last_entry: Option<Offset>,
    pub catalog_pins: usize,
    pub history_pins: usize,
    pub pending_receipts: u64,
    /// The oldest history version anything pins, below which collection may retire versions.
    pub pinned_floor: u64,
}

/// Asks the history owner to stop at its next turn boundary rather than mid-transaction; commands it
/// has not indexed are indexed from the log after the next start.
#[derive(Clone)]
pub(crate) struct Stopper {
    input: mpsc::Sender<Input>,
    stopped: Arc<AtomicBool>,
}

/// A fully rebuilt unpublished generation; factory implementations derive it from verified logs.
pub(crate) struct Replacement {
    pub id: StoreId,
    pub store: Store,
    pub index: Index,
}

pub(crate) type Rebuild = Box<dyn FnMut(&Head, &AtomicBool) -> Result<Replacement> + Send>;

pub(crate) struct Config {
    pub snapshots: SnapshotSection,
    pub domain: CheckpointDomain,
    pub key: Arc<EnclaveKey>,
    pub lifecycle: Arc<Lifecycle>,
    pub catalog_store: StoreId,
}

/// The node owns the forwarder; its blocking child owns every history mutation and certificate.
pub(crate) struct Runtime {
    actor: Actor,
    reader: Reader,
    input: mpsc::Sender<Input>,
    received: mpsc::Receiver<Input>,
    repair: mpsc::Receiver<StoreId>,
    stopped: Arc<AtomicBool>,
    endpoint: Endpoint,
    resume_at: Offset,
}

struct Actor {
    current: Replacement,
    generation: Arc<Generation>,
    published: watch::Sender<Option<Arc<Published>>>,
    ready: watch::Sender<bool>,
    ready_at: Option<u64>,
    cfg: SnapshotSection,
    domain: CheckpointDomain,
    key: Arc<EnclaveKey>,
    pins: Pins,
    /// The sequencer's next capture, handed over before the checkpoint it captures is published.
    seals: mpsc::Receiver<Box<Capture>>,
    pending: Option<Box<Capture>>,
    /// Shared with the sealing endpoint: whether a `snapshot_write_failed` episode may be open.
    write_alert: WriteAlert,
    /// Commands taken this turn with their derived rows, not yet indexed; one batch update indexes them.
    commands: Vec<Taken>,
    /// Shared with the sealing endpoint: how many physical entries this owner has taken, so a test
    /// can watch progress without joining the owner's queue.
    #[cfg(test)]
    taken: Arc<std::sync::atomic::AtomicU64>,
    /// Shared with the sealing endpoint: while set, the owner takes its input but indexes none of it,
    /// so a test holds the index behind the log.
    #[cfg(test)]
    index_held: Arc<AtomicBool>,
    retained_catalog: BTreeMap<Offset, Arc<crate::log::catalog::LazyCatalogView>>,
    last_entry: Option<Offset>,
    /// The checkpoint at `last_entry`, when the last entry taken is one.
    at_checkpoint: Option<crate::log::Checkpoint>,
    rebuild: Rebuild,
    stopped: Arc<AtomicBool>,
    lifecycle: Arc<Lifecycle>,
    last_certified: Option<crate::authenticated::Root>,
    /// The last command of the newest signed head.
    certified_seq: Option<u64>,
    /// The last command indexed when retired versions were last collected.
    collected_seq: Option<u64>,
    #[cfg(test)]
    inspections: Vec<oneshot::Sender<TestState>>,
}

impl Runtime {
    /// Activates only after the node has authorized persisted mutations and restored cache pins.
    /// Error cases:
    /// - required head proofs or publication construction fails
    pub(crate) fn new(current: Replacement, pins: Pins, config: Config, rebuild: Rebuild) -> Result<Self> {
        let Config { snapshots: cfg, domain, key, lifecycle, catalog_store } = config;
        let (ready, ready_rx) = watch::channel(false);
        let generation = Generation::new(current.id);
        let head = current.index.head();
        let resume_at = head.resume_at();
        let initial = Published::new(current.store.clone(), head, generation.clone())?;
        let (published, published_rx) = watch::channel(Some(initial));
        let (input, received) = mpsc::channel(super::QUEUE);
        let (seal_tx, seals) = mpsc::channel(1);
        let (repair_tx, repair) = mpsc::channel(1);
        let reader = Reader::authenticated(ready_rx, published_rx, repair_tx);
        let endpoint = Endpoint::new(seal_tx, catalog_store, cfg.interval);
        #[cfg(test)]
        let endpoint = endpoint.inspecting(input.clone());
        let stopped = Arc::new(AtomicBool::new(false));
        Ok(Self {
            actor: Actor {
                current,
                generation,
                published,
                ready,
                ready_at: None,
                cfg,
                domain,
                key,
                pins,
                seals,
                pending: None,
                write_alert: endpoint.write_alert.clone(),
                commands: Vec::new(),
                #[cfg(test)]
                taken: endpoint.taken.clone(),
                #[cfg(test)]
                index_held: endpoint.index_held.clone(),
                retained_catalog: BTreeMap::new(),
                last_entry: None,
                at_checkpoint: None,
                rebuild,
                stopped: stopped.clone(),
                lifecycle,
                last_certified: None,
                certified_seq: None,
                collected_seq: None,
                #[cfg(test)]
                inspections: Vec::new(),
            },
            reader,
            input,
            received,
            repair,
            endpoint,
            resume_at,
            stopped,
        })
    }

    /// Waits for the actual startup command tip; same-sequence physical fences remain in the feed.
    pub(crate) fn catch_up_to(&mut self, tip: Option<u64>) {
        self.actor.ready_at =
            tip.filter(|tip| self.actor.current.index.head().last_seq().is_none_or(|held| held < *tip));
        self.actor.ready.send_replace(self.actor.ready_at.is_none());
    }

    pub(crate) fn resume_at(&self) -> Offset {
        self.resume_at
    }
    pub(crate) fn reader(&self) -> Reader {
        self.reader.clone()
    }
    pub(crate) fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }
    pub(crate) fn stopper(&self) -> Stopper {
        Stopper { input: self.input.clone(), stopped: self.stopped.clone() }
    }
    #[cfg(test)]
    pub(crate) fn is_ready(&self) -> bool {
        *self.actor.ready.borrow()
    }

    /// Forwards every physical entry, including checkpoint fences, with an explicit stop wakeup; the
    /// owner takes each command's derived rows from `deriver`.
    /// Error cases:
    /// - verified backfill fails or the supervised storage owner stops
    pub(crate) async fn run(self, mut feed: Feed, deriver: Deriver) -> Result<()> {
        let Self { actor, input, received, mut repair, stopped, .. } = self;
        let guard = stop::Stop { input: input.clone(), stopped: stopped.clone() };
        let deriver = deriver.stopped_by(stopped.clone());
        // A test's collector sees the blocking owner's alerts; production logs to the global subscriber.
        #[cfg(test)]
        let collector = tracing::dispatcher::get_default(Clone::clone);
        let mut written = spawn_blocking(move || {
            #[cfg(test)]
            let _collector = tracing::dispatcher::set_default(&collector);
            actor.run(received, deriver)
        });
        let forward = async {
            let mut repair_open = true;
            loop {
                tokio::select! {
                    signal = repair.recv(), if repair_open => {
                        match signal {
                            Some(_) => if input.send(Input::Repair).await.is_err() { break; },
                            None => repair_open = false,
                        }
                    }
                    next = feed.next() => {
                        let Some(message) = next? else { break };
                        if input.send(Input::Entry(message)).await.is_err() { break; }
                    }
                }
            }
            stop::drain(&input, &stopped).await
        };
        let outcome = tokio::select! {
            result = forward => result,
            result = &mut written => {
                result.context("authenticated history owner panicked")??;
                if stopped.load(Ordering::Acquire) {
                    return Ok(());
                }
                bail!("authenticated history owner stopped");
            }
        };
        drop(guard);
        written.await.context("authenticated history owner panicked")??;
        outcome
    }
}

impl Actor {
    /// Serializes mutations and releases optional captures before returning from shutdown.
    /// Error cases:
    /// - a history mutation, proof, required repair or complete-head publication check fails
    fn run(mut self, mut input: mpsc::Receiver<Input>, mut deriver: Deriver) -> Result<()> {
        while !self.stopped.load(Ordering::Acquire) {
            let Some(first) = input.blocking_recv() else { break };
            self.take(first, &mut deriver)?;
            let mut taken = 1;
            while taken < super::QUEUE && !self.stopped.load(Ordering::Acquire) {
                let Ok(next) = input.try_recv() else { break };
                self.take(next, &mut deriver)?;
                taken += 1;
            }
            if self.stopped.load(Ordering::Acquire) {
                break;
            }
            self.index_commands()?;
            if taken == super::QUEUE {
                // A full turn: the input runs ahead of the owner, so a capture waiting for its checkpoint
                // takes the history indexed so far rather than wait for the backlog.
                self.complete_behind();
            }
            if self.certification_due() {
                self.certify_latest();
            }
            if self.collection_due() {
                self.maintain()?;
            }
            #[cfg(test)]
            self.reply_inspections();
        }
        self.pending.take();
        // A requested stop leaves the turn's commands to the next start, which indexes them from the
        // log after discarding anything unsigned.
        if !self.stopped.load(Ordering::Acquire) {
            self.index_commands()?;
        }
        self.certify_latest();
        Ok(())
    }

    /// Takes a capture waiting in its slot before `input`, which a checkpoint the capture names follows
    /// (`Slot::submit`). Repair signals are wakeups; current generation validity decides whether repair is
    /// needed.
    /// Error cases:
    /// - full verified repair or an exact command update fails
    fn take(&mut self, input: Input, deriver: &mut Deriver) -> Result<()> {
        if !self.generation.valid() {
            self.repair()?;
        }
        if let Ok(capture) = self.seals.try_recv() {
            self.seal(capture)?;
        }
        match input {
            Input::Entry(message) => self.entry(message, deriver)?,
            Input::Repair | Input::Stop => {}
            Input::Drain(done) => {
                self.index_commands()?;
                self.certify_latest();
                self.maintain()?;
                let _ = done.send(());
            }
            #[cfg(test)]
            Input::Inspect(done) => {
                self.inspections.push(done);
            }
            #[cfg(test)]
            Input::Pause(release) => {
                let _ = release.recv();
            }
        }
        Ok(())
    }

    /// Replies after the maintenance turn, while the owner is about to wait for its next input.
    #[cfg(test)]
    fn reply_inspections(&mut self) {
        for done in self.inspections.drain(..) {
            let _ = done.send(TestState {
                last_entry: self.last_entry,
                catalog_pins: self.retained_catalog.len(),
                history_pins: self.pins.len(),
                pending_receipts: self
                    .current
                    .store
                    .pending_receipts(&self.current.index.head().root)
                    .unwrap_or(u64::MAX),
                pinned_floor: self.current.store.pinned_floor(&self.current.index.head().root).unwrap_or(0),
            });
        }
    }

    /// Buffers commands, with their derived rows, for the turn's batch update; a captured checkpoint
    /// first indexes every command up to it, so its history head is exactly the checkpoint's command.
    /// Error cases:
    /// - verified history ingestion, publication, or feed ordering fails
    /// - a command's derived rows cannot be found (`Deriver::rows`)
    fn entry(&mut self, message: Arc<StreamMsg>, deriver: &mut Deriver) -> Result<()> {
        #[cfg(test)]
        self.taken.fetch_add(1, Ordering::AcqRel);
        if let StreamBody::Command(command) = &message.body {
            // A stop that ends a replay of the log leaves the command to the next start.
            let Some(taken) = deriver.taken(command)? else { return Ok(()) };
            self.commands.push(taken);
        }
        let at = message.offset;
        if self.pending.as_ref().is_some_and(|capture| capture.snapshot.offset <= at) {
            // A repair while indexing drops the capture, so take it only afterwards.
            self.index_commands()?;
        }
        self.at_checkpoint = match &message.body {
            StreamBody::Checkpoint(checkpoint) => Some(checkpoint.clone()),
            _ => None,
        };
        self.last_entry = Some(at);
        if let Some(capture) = self.pending.take_if(|capture| capture.snapshot.offset <= at) {
            let checkpoint = self.at_checkpoint.clone();
            self.complete_capture(capture, checkpoint.as_ref());
        }
        Ok(())
    }

    /// Indexes the buffered commands in updates of at most `INDEX_BATCH`, checking for a requested
    /// stop between them, and publishes the resulting complete version.
    /// Error cases:
    /// - verified ingestion fails for a reason a fresh generation cannot fix, or repair fails
    fn index_commands(&mut self) -> Result<()> {
        #[cfg(test)]
        if self.index_held.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.commands.is_empty() {
            return Ok(());
        }
        let mut commands = std::mem::take(&mut self.commands);
        while !commands.is_empty() && !self.stopped.load(Ordering::Acquire) {
            let batch: Vec<_> = commands.drain(..commands.len().min(INDEX_BATCH)).collect();
            if let Err(error) = self.current.index.ingest_taken(&batch) {
                if !self.current.index.requires_rebuild() {
                    return Err(error);
                }
                self.damaged(&error);
                self.repair()?;
                self.current.index.ingest_taken(&batch)?;
            }
        }
        if let Err(error) = self.publish() {
            self.damaged(&error);
            self.repair()?;
        }
        Ok(())
    }

    /// Publishes one immutable complete version; readers keep their previous versions independently.
    /// Error cases:
    /// - fixed metadata or version leasing fails
    fn publish(&mut self) -> Result<()> {
        let head = self.current.index.head();
        self.published.send_replace(Some(Published::new(
            self.current.store.clone(),
            head.clone(),
            self.generation.clone(),
        )?));
        if self.ready_at.is_none_or(|tip| head.last_seq().is_some_and(|held| held >= tip)) {
            self.ready_at = None;
            self.ready.send_replace(true);
        }
        Ok(())
    }

    /// Withholds successful future pages before any replacement is built from the public log.
    fn damaged(&self, error: &anyhow::Error) {
        self.generation.invalidate();
        self.ready.send_replace(false);
        warn!(alert = "history_index_damaged", "withholding pages until verified repair: {error:#}");
    }

    /// Replaces no row or authority in the damaged generation; old readers retain its actual owner.
    /// Error cases:
    /// - fresh log-derived rebuilding, verification, or publication fails
    fn repair(&mut self) -> Result<()> {
        self.ready.send_replace(false);
        self.pending.take();
        let expected = self.current.index.head();
        let replacement =
            (self.rebuild)(&expected, &self.stopped).context("rebuilding damaged authenticated history")?;
        ensure!(
            replacement.index.head().command_prefix == expected.command_prefix,
            "repair returned a different command prefix"
        );
        ensure!(
            replacement.index.head().last_seq() == expected.last_seq()
                && replacement.index.head().last_ts() == expected.last_ts()
                && replacement.index.head().retention_ms == expected.retention_ms,
            "repair returned different history progress"
        );
        let generation = Generation::new(replacement.id);
        let published = Published::new(replacement.store.clone(), replacement.index.head(), generation.clone())?;
        let mut pins = Pins::new(replacement.id, replacement.store.clone(), self.lifecycle.payloads());
        pins.protect(&replacement.index.head().root)?;
        self.current = replacement;
        self.generation = generation;
        self.pins = pins;
        self.last_certified = None;
        self.published.send_replace(Some(published));
        self.publish()?;
        self.certify_latest();
        info!(alert_resolved = "history_index_damaged", "verified history generation rebuilt");
        Ok(())
    }
}
