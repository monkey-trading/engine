//! Shared test harness: scratch directories, the committed dev config, a running sequencer, a
//! history index following it, and a log layer that hands every alert to the test.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_core::auth::ClientAuth;
use engine_core::command::Command;
use engine_core::event::{CancelReason, Event, Fill, Reject};
use engine_core::price::PriceError;
use engine_core::types::{AccountId, Side};
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
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

use crate::api::{Feed, MAX_BACKFILLS};
use crate::clock::{Clock, SystemClock};
use crate::config::{HistorySection, NodeConfig};
use crate::history::{History, HistoryCheck, HistoryEvent, Reader};
use crate::log::{self, LogEntry, LogReader, Manifest, Offset, SegmentedLog};
use crate::metrics::Metrics;
use crate::sequencer::{Request, Sequencer, SequencerConfig, SignedWithdrawal, StreamMsg, genesis_commands, replay};
use crate::tee::EnclaveKey;
pub use crate::test_tracing::keep_callsites_interested;
use crate::trusted_time::Sample;

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
    task: JoinHandle<anyhow::Result<()>>,
}

impl TestSequencer {
    /// Recovers a sequencer from `dir` and boots it as the node does: records client auth and
    /// activates the latest protocol, sequences the dev genesis if the log lacks it, and runs it.
    /// The stream channel holds `stream_capacity` messages.
    pub fn start(dir: &Path, stream_capacity: usize) -> Self {
        Self::start_at(dir, stream_capacity, Arc::new(SystemClock))
    }

    /// As `start`, stamping commands on `clock`, which trusted time admits.
    pub fn start_at(dir: &Path, stream_capacity: usize, clock: Arc<dyn Clock>) -> Self {
        Self::resume_at(dir, stream_capacity, clock, genesis_commands(&dev_config().genesis))
    }

    /// As `start_at`, on a log that finished `genesis` rather than the dev config's.
    pub fn resume_at(dir: &Path, stream_capacity: usize, clock: Arc<dyn Clock>, genesis: Vec<Command>) -> Self {
        let (stream, _) = broadcast::channel(stream_capacity);
        let (withdrawals_tx, withdrawals) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let key = Arc::new(EnclaveKey::test_key());
        let metrics = Arc::new(Metrics::new());
        let mut cfg =
            SequencerConfig { clock: clock.clone(), metrics: metrics.clone(), ..sequencer_config(dir, manifest_tx) };
        cfg.genesis = genesis;
        let mut seq = Sequencer::recover(cfg, key, stream.clone(), withdrawals_tx).expect("sequencer recovers");
        // As the node does, parameters an older log set take the config's notional weighting first.
        let weighting = dev_config().genesis.params.toxicity.notional.expect("the dev config weighs by notional");
        seq.carry_toxicity_notional(weighting).expect("toxicity notional");
        seq.activate_protocol().expect("protocol activation");
        if seq.needs_genesis() {
            seq.genesis(&dev_config().genesis).expect("dev genesis");
        }
        // Trusted time admits stamps on `clock`, so the queued genesis goes in now.
        seq.trust(sample_around(clock.as_ref())).expect("dev genesis");
        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(seq.run(rx));
        Self { tx, stream, manifest, withdrawals, metrics, task }
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
        let cfg = HistorySection { dir: index.to_owned(), retention: RETENTION };
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

/// An operator public key nobody has published: the uncompressed SEC1 hex of a fresh random key.
pub fn own_operator_key() -> String {
    let key = SigningKey::generate();
    format!("0x{}", hex::encode(key.verifying_key().to_sec1_point(false).as_bytes()))
}

/// The genesis `entries` ran, as a dev node that resumes the log is configured with it.
pub fn finished_genesis(entries: &[LogEntry]) -> Vec<Command> {
    replay(entries).expect("the log replays").genesis.as_config(dev_config().genesis.params.toxicity.notional)
}

/// The committed dev config.
pub fn dev_config() -> NodeConfig {
    NodeConfig::load(Path::new(DEV_CONFIG)).expect("dev config loads")
}

/// The sequencer settings the harness runs with, publishing the manifest on `manifest`.
pub fn sequencer_config(dir: &Path, manifest: watch::Sender<Manifest>) -> SequencerConfig<'_> {
    SequencerConfig {
        log_dir: dir,
        segment_commands: SEGMENT_COMMANDS,
        manifest,
        checkpoint_every: CHECKPOINT_EVERY,
        checkpoint_registry: None,
        checkpoint_parent: Default::default(),
        checkpoints: None,
        chain_id: 31337,
        vault: AccountId([0; 20]),
        clock: Arc::new(SystemClock),
        max_drift: Duration::from_secs(10),
        client_auth: ClientAuth::Unsigned,
        operator: dev_config().operator().expect("the dev operator parses"),
        metrics: Arc::new(Metrics::new()),
        genesis: genesis_commands(&dev_config().genesis),
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

/// The manifest on disk, as a writer that has flushed everything on disk publishes it.
pub fn published_manifest(dir: &Path) -> watch::Receiver<Manifest> {
    let flushed = Some(Offset { seq: u64::MAX, index: u32::MAX });
    watch::channel(Manifest { flushed, ..Manifest::load(dir).expect("manifest loads") }).1
}

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
        .filter(|e| serde_json::to_value(&e.event).expect("event encodes")["account"] == account.to_string())
        .collect();
    events.reverse();
    events
}

/// One event of each kind, and an order rejection for each reason. The matches stop compiling when
/// a variant is added, until it has a sample here.
pub fn every_event() -> Vec<Event> {
    let (account, amount, market) = (AccountId([1; 20]), Fixed::from_int(5), 2);
    let reasons = [
        Reject::UnknownMarket,
        Reject::UnknownOrder,
        Reject::InvalidOrder { reason: "qty".into() },
        Reject::MarketClosed,
        Reject::ReduceOnly,
        Reject::NoPrice { error: PriceError::Stale { fresh: 1, needed: 2 } },
        Reject::NoPrice { error: PriceError::Divergent { divergence_bps: 30 } },
        Reject::NoFxPrice { fx_market: 10 },
        Reject::Slippage { quoted: amount, live: amount, slippage_bps: 5 },
        Reject::InsufficientMargin { equity: amount, required: amount },
        Reject::InsufficientFunds { available: amount },
        Reject::HouseCapacity,
        Reject::TooManyOrders { max: 100 },
        Reject::InvalidAmount,
        Reject::DuplicateReference,
        Reject::MissingSignature,
        Reject::InvalidSignature,
        Reject::InvalidNonce { expected: 3 },
        Reject::Expired { expires_at: 1_000 },
        Reject::ExpiryTooFar { latest: 2_000 },
    ];
    for reason in &reasons {
        match reason {
            Reject::UnknownMarket
            | Reject::UnknownOrder
            | Reject::InvalidOrder { .. }
            | Reject::MarketClosed
            | Reject::ReduceOnly
            | Reject::NoPrice { .. }
            | Reject::NoFxPrice { .. }
            | Reject::Slippage { .. }
            | Reject::InsufficientMargin { .. }
            | Reject::InsufficientFunds { .. }
            | Reject::HouseCapacity
            | Reject::TooManyOrders { .. }
            | Reject::InvalidAmount
            | Reject::DuplicateReference
            | Reject::MissingSignature
            | Reject::InvalidSignature
            | Reject::InvalidNonce { .. }
            | Reject::Expired { .. }
            | Reject::ExpiryTooFar { .. } => {}
        }
    }
    let fill = Fill {
        account,
        order_id: 3,
        market,
        side: Side::Sell,
        qty: amount,
        price: amount,
        index: amount,
        spread_bps: 4,
        realised_pnl: -amount,
        fx_index: Some(amount),
        profit_fee: Fixed::ZERO,
        position: amount,
        liquidation: true,
    };
    let mut events: Vec<_> =
        reasons.into_iter().map(|reason| Event::OrderRejected { account, client_id: 1, reason }).collect();
    events.extend([
        Event::Deposited { account, amount },
        Event::DepositRejected { account, amount, chain_ref: "0xab:1".into(), reason: Reject::DuplicateReference },
        Event::WithdrawalApproved { account, amount, nonce: 1 },
        Event::WithdrawalRejected { account, amount, reason: Reject::InsufficientFunds { available: amount } },
        Event::HouseCapitalMoved { amount, nav: amount },
        Event::OrderAccepted { account, client_id: 1, order_id: 3 },
        Event::OrderCancelled { account, order_id: 3, reason: CancelReason::Client },
        Event::OrderCancelled { account, order_id: 3, reason: CancelReason::Unfillable },
        Event::Fill(fill),
        Event::CloseOut { account, equity: amount, margin: amount },
        Event::BadDebt { account, amount },
        Event::Financing { account, market, amount: -amount },
        Event::BenchmarkRateMissing { benchmark: "SOFR".into(), markets: vec![market] },
        Event::MarketOpened { market },
        Event::MarketClosed { market },
        Event::StressChanged { market, level: 2 },
        Event::PricesRejected { market, reason: "stale quote".into() },
        Event::AdminApplied,
        Event::AdminRejected { reason: "bad params".into() },
    ]);
    for event in &events {
        match event {
            Event::Deposited { .. }
            | Event::DepositRejected { .. }
            | Event::WithdrawalApproved { .. }
            | Event::WithdrawalRejected { .. }
            | Event::HouseCapitalMoved { .. }
            | Event::OrderAccepted { .. }
            | Event::OrderRejected { .. }
            | Event::OrderCancelled { .. }
            | Event::Fill(_)
            | Event::CloseOut { .. }
            | Event::BadDebt { .. }
            | Event::Financing { .. }
            | Event::BenchmarkRateMissing { .. }
            | Event::BenchmarkRateObserved { .. }
            | Event::BenchmarkRateUndated { .. }
            | Event::MarketOpened { .. }
            | Event::MarketClosed { .. }
            | Event::StressChanged { .. }
            | Event::PricesRejected { .. }
            | Event::AdminApplied
            | Event::AdminRejected { .. } => {}
        }
    }
    events
}

/// A log writer that keeps what it is given.
#[derive(Clone, Default)]
pub struct Written(Arc<Mutex<Vec<u8>>>);

impl io::Write for Written {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'w> MakeWriter<'w> for Written {
    type Writer = Written;

    fn make_writer(&'w self) -> Self::Writer {
        self.clone()
    }
}

impl Written {
    /// Everything written so far.
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}
