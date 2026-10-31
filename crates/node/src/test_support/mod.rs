//! Shared test harness: scratch directories, the committed dev config, a running sequencer, a
//! history index following it, and a log layer that hands every alert to the test.

mod events;
pub use events::every_event;

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engine_core::auth::ClientAuth;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};
use futures_util::FutureExt;
use k256::ecdsa::SigningKey;
use k256::elliptic_curve::Generate;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::field::{Field, Visit};
use tracing::subscriber::DefaultGuard;
use tracing::{Event as LogEvent, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

mod written;
pub use written::Written;

use crate::api::{Feed, MAX_BACKFILLS, Signing};
use crate::clock::test_clock::TestClock;
use crate::clock::{Clock, SystemClock};
use crate::config::{HistorySection, NodeConfig};
use crate::history::{History, HistoryCheck, HistoryEvent, Reader};
use crate::log::{self, LogEntry, LogReader, Manifest, Offset, SegmentedLog};
use crate::metrics::Metrics;
use crate::sequencer::{Request, Sequencer, SequencerConfig, SignedWithdrawal, StreamMsg, genesis_commands};
use crate::tee::EnclaveKey;
pub use crate::test_tracing::keep_callsites_interested;
#[path = "../../tests/common/access.rs"]
pub(crate) mod access;
use crate::trusted_time::{Sample, TrustedClock};

pub const DEV_CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/dev.toml");
pub const ANVIL_CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/anvil.toml");
/// Small, so a short test crosses several segment boundaries.
pub const SEGMENT_COMMANDS: u64 = 4;
pub const CHECKPOINT_EVERY: u64 = 3;
pub const TRADER: AccountId = AccountId([7; 20]);
/// Long enough that no test event leaves the history index.
pub const RETENTION: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);

/// One alert's fields, by name, and its `level`.
pub type Alert = BTreeMap<String, String>;

/// A sequencer task running on a log directory, with the ends of its channels.
pub struct TestSequencer {
    pub tx: mpsc::Sender<Request>,
    pub stream: broadcast::Sender<Arc<StreamMsg>>,
    pub manifest: watch::Receiver<Manifest>,
    /// What custody receives.
    pub withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>,
    pub metrics: Arc<Metrics>,
    marks_rx: Option<mpsc::Receiver<crate::history::marks::MarkSample>>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl TestSequencer {
    /// Boots the current protocol as the node does, with the source left unconfigured.
    pub fn start(dir: &Path, stream_capacity: usize) -> Self {
        Self::start_at(dir, stream_capacity, Arc::new(SystemClock), genesis_commands(&dev_config().genesis))
    }

    /// As `start`, stamping commands on `clock`, which trusted time admits, on a log whose genesis is
    /// `genesis`, as the config gives it.
    pub fn start_at(dir: &Path, stream_capacity: usize, clock: Arc<dyn Clock>, genesis: Vec<Command>) -> Self {
        Self::boot(dir, stream_capacity, clock, genesis, ProtocolVersion::LATEST)
    }

    /// As `start`, activating `V18`: a version that signs each withdrawal when it is approved, as logs
    /// written before `V22` do.
    pub fn start_v18(dir: &Path, stream_capacity: usize) -> Self {
        let genesis = genesis_commands(&dev_config().genesis);
        Self::boot(dir, stream_capacity, Arc::new(SystemClock), genesis, ProtocolVersion::V18)
    }

    /// Boots an explicit V23 fixture for historical unsigned/admission/storage assertions, the last before access screening.
    pub fn start_legacy(dir: &Path, stream_capacity: usize) -> Self {
        Self::start_legacy_at(dir, stream_capacity, Arc::new(SystemClock), genesis_commands(&dev_config().genesis))
    }

    /// Historical V23 fixture on a controlled clock, on a log whose genesis is `genesis`.
    pub fn start_legacy_at(dir: &Path, stream_capacity: usize, clock: Arc<dyn Clock>, genesis: Vec<Command>) -> Self {
        Self::boot(dir, stream_capacity, clock, genesis, ProtocolVersion::V23)
    }

    /// Boots as the node does, activating `version`.
    fn boot(
        dir: &Path,
        stream_capacity: usize,
        clock: Arc<dyn Clock>,
        genesis: Vec<Command>,
        version: ProtocolVersion,
    ) -> Self {
        let (stream, _) = broadcast::channel(stream_capacity);
        let (withdrawals_tx, withdrawals) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let key = Arc::new(EnclaveKey::test_key());
        let cfg = SequencerConfig { clock: clock.clone(), genesis, ..sequencer_config(dir, manifest_tx) };
        let metrics = cfg.metrics.clone();
        let mut seq = Sequencer::recover(cfg, key, stream.clone(), withdrawals_tx).expect("sequencer recovers");
        // As the node does, parameters an older log set take the config's notional weighting first.
        let weighting = dev_config().genesis.params.toxicity.notional.expect("the dev config weighs by notional");
        seq.carry_toxicity_notional(weighting).expect("toxicity notional");
        if version == ProtocolVersion::LATEST {
            seq.activate_protocol().expect("protocol activation");
        } else {
            seq.activate_protocol_at(version).expect("protocol activation");
        }
        if seq.needs_genesis() {
            seq.genesis(&dev_config().genesis).expect("dev genesis");
        }
        // Trusted time admits stamps on `clock`, so the queued genesis goes in now.
        seq.trust(sample_around(clock.as_ref())).expect("dev genesis");
        let (marks_tx, marks_rx) = mpsc::channel(crate::history::marks::run::QUEUE);
        seq.track_marks(marks_tx);
        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(seq.run(rx));
        Self { tx, stream, manifest, withdrawals, metrics, marks_rx: Some(marks_rx), task }
    }

    pub fn take_marks(&mut self) -> mpsc::Receiver<crate::history::marks::MarkSample> {
        self.marks_rx.take().expect("taken once")
    }

    /// Stops the sequencer as a shutdown does: it drains and writes a final checkpoint.
    pub async fn stop(self) {
        drop(self.tx);
        self.task.await.expect("sequencer task").expect("sequencer drains");
    }

    /// Sequences `cmd` and waits until it is flushed and published.
    pub async fn submit(&self, cmd: Command) -> Vec<Event> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Request::Submit { cmd, reply: Some(reply) }).await.expect("sequencer running");
        rx.await.expect("sequencer replied")
    }

    /// The seq the next command will get.
    pub async fn next_seq(&self) -> u64 {
        let (reply, rx) = oneshot::channel();
        let query = Box::new(move |e: &Engine| {
            let _ = reply.send(e.last_seq().map_or(0, |s| s + 1));
        });
        self.tx.send(Request::Query(query)).await.expect("sequencer running");
        rx.await.expect("sequencer replied")
    }

    /// Sequences ticks until the next command will be followed by a checkpoint, and returns its
    /// seq.
    pub async fn tick_to_checkpoint(&self) -> u64 {
        loop {
            let seq = self.next_seq().await;
            if (seq + 1).is_multiple_of(CHECKPOINT_EVERY) {
                return seq;
            }
            self.submit(Command::Tick).await;
        }
    }
}

/// A history index following a `TestSequencer`'s log: its reader, where it resumed, and its task.
pub struct TestHistory {
    pub reader: Reader,
    pub resume_at: Offset,
    task: JoinHandle<anyhow::Result<()>>,
}

impl TestHistory {
    /// Opens the index in `index` against the log `node` writes in `dir`, and runs it, letting
    /// readers in once it has taken the log's last command now.
    /// Error cases:
    /// - the index cannot be opened, nor a fresh one in its place (see `HistoryCheck`)
    pub async fn start(node: &TestSequencer, dir: &Path, index: &Path) -> anyhow::Result<Self> {
        let tip = node.next_seq().await.checked_sub(1);
        Self::start_with_tip(node, dir, index, tip)
    }

    /// As `start`, but readers are let in once the index has taken `tip`.
    /// Error cases:
    /// - the index cannot be opened, nor a fresh one in its place (see `HistoryCheck`)
    pub fn start_with_tip(node: &TestSequencer, dir: &Path, index: &Path, tip: Option<u64>) -> anyhow::Result<Self> {
        let cfg = HistorySection { dir: index.to_owned(), retention: RETENTION, marks: None };
        let mut history = open_history(&cfg, dir)?;
        history.catch_up_to(tip);
        let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
        let (reader, resume_at) = (history.reader(), history.resume_at());
        let feed =
            Feed::new(node.stream.subscribe(), dir.to_owned(), node.manifest.clone(), backfills, Some(resume_at));
        Ok(Self { reader, resume_at, task: tokio::spawn(history.run(feed)) })
    }

    /// The task's result once it has taken the whole log, which it ends with after the sequencer
    /// stops.
    /// Error cases:
    /// - the history task failed (see `History::run`)
    pub async fn finish(self) -> anyhow::Result<()> {
        timeout(Duration::from_secs(10), self.task).await.expect("the history task ends in time").expect("history task")
    }
}

/// Collects the fields of every event that carries an `alert` field.
struct AlertLayer(mpsc::UnboundedSender<Alert>);

/// Records an event's fields as strings.
#[derive(Default)]
struct Fields(Alert);

impl<S: Subscriber> Layer<S> for AlertLayer {
    fn on_event(&self, event: &LogEvent<'_>, _: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.0.contains_key("alert") {
            fields.0.insert("level".to_owned(), event.metadata().level().to_string());
            let _ = self.0.send(fields.0);
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

/// Polls `read` once, so that it takes one of `permits` and starts blocking work that cannot finish
/// yet, then drops it: the blocking work must keep the permit until `release` lets it end. It
/// releases before asserting, so a failure never leaves the work blocked.
pub async fn assert_a_dropped_read_keeps_its_permit(permits: &Semaphore, read: impl Future, release: impl FnOnce()) {
    let free = permits.available_permits();
    let mut read = Box::pin(read);
    let waiting = (&mut read).now_or_never().is_none();
    drop(read);
    let held = free - permits.available_permits();
    release();
    assert!(waiting, "the read waits on its blocking work");
    assert_eq!(held, 1, "the blocking work still holds the permit");
    let every = u32::try_from(free).expect("few permits");
    let back = timeout(Duration::from_secs(10), permits.acquire_many(every)).await;
    drop(back.expect("the permit comes back once the work ends").expect("the semaphore is open"));
}

/// How long a test waits for work that finishes in seconds on an idle machine. A wait ends the
/// moment its condition holds, so the ceiling only bounds a hang on a loaded runner.
pub const LOADED_WAIT: Duration = Duration::from_secs(180);
/// The vault's exit period, claim window and deposit staleness, in seconds, for local scenarios
/// that do not exercise the exit: thirty days each, longer than any of them waits.
pub const DEV_EXIT_SCHEDULE: (u64, u64, u64) = (2_592_000, 2_592_000, 2_592_000);

/// Awaits `condition` for up to `LOADED_WAIT`, panicking with `what` it waited for on a miss.
pub async fn wait_for<T>(what: &str, condition: impl Future<Output = T>) -> T {
    timeout(LOADED_WAIT, condition).await.unwrap_or_else(|_| panic!("gave up after {LOADED_WAIT:?} waiting for {what}"))
}

/// Hands the sequencer a fresh system-clock time sample every second until it stops, so a test that
/// outlasts the sequencer's no-sample grace on a loaded machine is not refused its stamps. It holds
/// only a weak sender, so it never keeps the sequencer's queue open.
pub async fn keep_time_fresh(tx: mpsc::WeakSender<Request>) {
    let mut every = tokio::time::interval(Duration::from_secs(1));
    loop {
        every.tick().await;
        let Some(tx) = tx.upgrade() else { return };
        if tx.send(Request::TimeSample(trusted_now())).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_time_refresher_samples_every_second_and_ends_with_the_queue() {
        let (tx, mut rx) = mpsc::channel(8);
        let refresher = tokio::spawn(keep_time_fresh(tx.downgrade()));
        for _ in 0..3 {
            let sample = timeout(Duration::from_secs(2), rx.recv()).await.expect("a sample arrives each second");
            assert!(matches!(sample, Some(Request::TimeSample(_))));
        }
        assert!(rx.try_recv().is_err(), "one sample per second, no more");
        drop(tx);
        refresher.await.unwrap();
    }
}

/// Routes this thread's logs to an alert collector until the guard drops. `#[tokio::test]` runs
/// every task on the test's thread, so the alerts of the tasks a test spawns all arrive.
pub fn capture_alerts() -> (DefaultGuard, mpsc::UnboundedReceiver<Alert>) {
    keep_callsites_interested();
    let (tx, rx) = mpsc::unbounded_channel();
    (tracing::subscriber::set_default(Registry::default().with(AlertLayer(tx))), rx)
}

/// An empty directory for one test, named after it.
pub fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("engine-node-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A marks-cache reader over a scratch store no test reads; one directory per calling thread.
pub fn marks_reader() -> crate::history::marks::Reader {
    use crate::history::marks::{Retentions, Store};
    let dir = scratch_dir(&format!("unused-marks-{:?}", std::thread::current().id()));
    Store::open_in(&dir, Retentions::defaults(), [0x42; 32]).expect("scratch").reader()
}

/// An operator public key nobody has published: the uncompressed SEC1 hex of a fresh random key.
pub fn own_operator_key() -> String {
    let key = SigningKey::generate();
    format!("0x{}", hex::encode(key.verifying_key().to_sec1_point(false).as_bytes()))
}

/// The committed dev config.
pub fn dev_config() -> NodeConfig {
    NodeConfig::load(Path::new(DEV_CONFIG)).expect("dev config loads")
}

/// The dev config's answer signing, by the published mock enclave key, at a time trusted time
/// bounds: the system's when it was made, on a test clock that stands still.
pub fn signing() -> Arc<Signing> {
    signing_with(trusted(Arc::new(TestClock::default())))
}

/// The dev config's answer signing, by the published mock enclave key, at the times `time` reads.
pub fn signing_with(time: TrustedClock) -> Arc<Signing> {
    let cfg = dev_config();
    let vault = cfg.custody.vault.parse().expect("the dev vault parses");
    let key = Arc::new(EnclaveKey::test_key());
    Arc::new(Signing::new(key, cfg.custody.chain_id, vault, cfg.api.heartbeat_interval, time))
}

/// `clock`, bounded under the dev config's drift by a sample around its reading now, which ages only
/// as a test clock is advanced.
pub fn trusted(clock: Arc<dyn Clock>) -> TrustedClock {
    let sample = sample_around(clock.as_ref());
    TrustedClock::new(clock, watch::channel(Some(sample)).1, dev_config().time.max_drift)
}

/// The sequencer settings the harness runs with, publishing the manifest on `manifest`.
pub fn sequencer_config(dir: &Path, manifest: watch::Sender<Manifest>) -> SequencerConfig<'_> {
    SequencerConfig {
        access_policy: None,
        log_dir: dir,
        segment_commands: SEGMENT_COMMANDS,
        manifest,
        checkpoint_every: CHECKPOINT_EVERY,
        checkpoint_registry: None,
        approval_registry: None,
        checkpoint_parent: Default::default(),
        checkpoints: None,
        history_sealing: None,
        checkpoint_signing: None,
        chain_id: 31337,
        vault: AccountId([0; 20]),
        clock: Arc::new(SystemClock),
        max_drift: Duration::from_secs(10),
        client_auth: ClientAuth::Unsigned,
        operator: dev_config().operator().expect("the dev operator parses"),
        metrics: Arc::new(Metrics::new()),
        genesis: genesis_commands(&dev_config().genesis),
        close_spread_ceilings: dev_config().genesis.close_spread_ceilings.clone(),
    }
}

/// A trusted time sample putting true time within a second of `clock`'s wall clock, now.
pub fn sample_around(clock: &dyn Clock) -> Sample {
    let now = clock.now_ms();
    Sample { at: clock.instant(), earliest_ms: now - 1_000, latest_ms: now + 1_000 }
}

/// `sample_around` the system clock.
pub fn trusted_now() -> Sample {
    sample_around(&SystemClock)
}

/// A fixture's authenticated scan of an empty vault before any receipt.
pub fn empty_custody_frontier() -> Command {
    Command::CustodyFinalised(engine_core::deposits::CustodyCheckpoint {
        block: 0,
        block_hash: engine_core::eip712::B256::repeat_byte(1),
        deposit_count: 0,
    })
}

/// A deposit to `TRADER` of `amount` whole units under `chain_ref`.
pub fn deposit(amount: i64, chain_ref: &str) -> Command {
    Command::Deposit { account: TRADER, amount: Fixed::from_int(amount), chain_ref: chain_ref.to_owned() }
}

/// A withdrawal by `TRADER` of `amount` whole units.
pub fn withdraw(amount: i64) -> Command {
    Command::Withdraw { account: TRADER, amount: Fixed::from_int(amount), auth: None }
}

/// Every message on disk at or after `from`, as the stream encodes them.
pub fn log_messages(dir: &Path, from: Offset) -> Vec<String> {
    let mut reader = LogReader::open(dir, from, published_manifest(dir)).expect("log opens");
    std::iter::from_fn(|| reader.next_entry().expect("log reads"))
        .map(|(offset, entry)| serde_json::to_string(&StreamMsg::new(offset, entry)).expect("message encodes"))
        .collect()
}

mod log_view;
pub use log_view::published_manifest;

/// Every entry of the log in `dir`, read as recovery reads it.
pub fn log_entries(dir: &Path) -> Vec<LogEntry> {
    let mut entries = Vec::new();
    SegmentedLog::open(dir, |entry| {
        entries.push(entry);
        Ok(())
    })
    .expect("log opens");
    entries
}

/// Every entry of the single log file at `path`.
pub fn file_entries(path: &Path) -> Vec<LogEntry> {
    let mut entries = Vec::new();
    log::read_file(path, |entry| {
        entries.push(entry);
        Ok(())
    })
    .expect("log file reads");
    entries
}

/// The history index under `cfg`, checked against the log in `dir` as recovery checks it.
/// Error cases:
/// - the index cannot be opened, or does not match the log (see `HistoryCheck::finish`)
/// - the log cannot be read
pub fn open_history(cfg: &HistorySection, dir: &Path) -> anyhow::Result<History> {
    keep_callsites_interested();
    let mut check = HistoryCheck::open(cfg)?;
    SegmentedLog::open(dir, |entry| {
        check.observe(&entry);
        Ok(())
    })?;
    check.finish()
}

/// Every event in the log in `dir` whose JSON names `account`, newest first, as history serves it.
pub fn logged_history(dir: &Path, account: AccountId) -> Vec<HistoryEvent> {
    let entries = log_entries(dir);
    let mut events: Vec<HistoryEvent> = entries
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(command) => Some(command),
            _ => None,
        })
        .flat_map(|command| {
            let (seq, ts) = (command.sequenced.seq, command.sequenced.ts);
            command.events.into_iter().flatten().enumerate().map(move |(index, event)| {
                let cursor = Offset { seq, index: u32::try_from(index).expect("few events per command") };
                HistoryEvent { cursor, ts, event }
            })
        })
        .filter(|e| {
            let json = serde_json::to_value(&e.event).expect("event encodes");
            json.get("account").or_else(|| json.pointer("/receipt/beneficiary/account"))
                == Some(&serde_json::Value::String(account.to_string()))
        })
        .collect();
    events.reverse();
    events
}

/// A checkpoint poster over `chain` that retries after a millisecond and escalates an uncredited
/// deposit after a second.
pub fn poster<R>(chain: R, domain: crate::log::CheckpointDomain, signer: AccountId) -> crate::checkpoints::Poster<R> {
    crate::checkpoints::Poster {
        registry: None,
        chain,
        domain,
        signer,
        retry: Duration::from_millis(1),
        pending_deposits_critical_after: Duration::from_secs(1),
        post_withheld: Default::default(),
    }
}
