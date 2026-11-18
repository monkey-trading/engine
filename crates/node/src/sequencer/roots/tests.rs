//! The root worker as the sequencer's `run` loop drives it (#925): paced checkpoints deferred for root
//! work and taken with nothing arriving, unpaced ones never deferred, payout proofs read on the worker,
//! proof reads answered only from checkpoints the log holds durably, the tree handed over at `V9`'s
//! first root and across a discarding activation, and the sequencer stopping with its worker. Each test
//! runs the sequencer on its own thread, as the node does, and reads what it logs there.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::merkle::AccountProof;
use engine_core::types::{AccountId, Millis};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::timeout;
use tracing::field::{Field, Visit};
use tracing::{Event as LogEvent, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

use crate::checkpoints::{Health, Work};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::log::{Checkpoint, LogEntry, Manifest};
use crate::sequencer::{Request, Sequencer, SequencerConfig, SignedWithdrawal, StreamBody, StreamMsg};
use crate::tee::EnclaveKey;
use crate::test_support::{
    dev_config, empty_custody_frontier, keep_callsites_interested, log_entries, sample_around, scratch_dir,
    sequencer_config,
};

mod deferral;
mod lifecycle;
mod payouts;
mod reads;
mod work;

/// How long a test waits for what takes well under a second on an idle machine: only a hang reaches it.
const WAIT: Duration = Duration::from_secs(60);

/// The test clock, counting the sequencer's reads of monotonic time. `run` reads it once for every
/// batch it takes, so a count that stands still is a sequencer that takes nothing.
#[derive(Default)]
struct CountingClock {
    inner: TestClock,
    instants: AtomicUsize,
}

impl Clock for CountingClock {
    fn now_ms(&self) -> Millis {
        self.inner.now_ms()
    }

    fn instant(&self) -> Instant {
        self.instants.fetch_add(1, Ordering::SeqCst);
        self.inner.instant()
    }
}

/// An event the sequencer logged on its thread: its fields by name, with `level` and `message`.
type Logged = BTreeMap<String, String>;

/// Sends every event logged on the thread it is installed on to the test.
struct Recorder(mpsc::UnboundedSender<Logged>);

/// An event's fields, as strings.
#[derive(Default)]
struct Fields(Logged);

impl<S: Subscriber> Layer<S> for Recorder {
    fn on_event(&self, event: &LogEvent<'_>, _: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        fields.0.insert("level".to_owned(), event.metadata().level().to_string());
        let _ = self.0.send(fields.0);
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

/// `logged`'s field `name` as a number.
fn number(logged: &Logged, name: &str) -> u64 {
    logged.get(name).and_then(|value| value.parse().ok()).unwrap_or_else(|| panic!("no number {name} in {logged:?}"))
}

/// Whether `logged` is the line `message`.
fn says(logged: &Logged, message: &str) -> bool {
    logged.get("message").is_some_and(|said| said == message)
}

/// The `index`th funded account.
fn account(index: u32) -> AccountId {
    let mut bytes = [3; 20];
    bytes[16..].copy_from_slice(&index.to_be_bytes());
    AccountId(bytes)
}

/// A sequencer at `V23`, whose commands need no client signature, on a fresh log past its genesis and
/// an empty custody scan, with that scan checkpointed and nothing on a schedule after it. With `gate`,
/// the payout gate's checkpoints go there, and collateral has reconciled.
struct Node {
    seq: Sequencer,
    dir: PathBuf,
    clock: Arc<CountingClock>,
    gate: Option<mpsc::Sender<Work>>,
    signed: mpsc::UnboundedReceiver<SignedWithdrawal>,
}

impl Node {
    fn new(name: &str, gate: Option<mpsc::Sender<Work>>) -> Self {
        let dir = scratch_dir(name);
        let clock = Arc::new(CountingClock::default());
        let (withdrawals, signed) = mpsc::unbounded_channel();
        let cfg = Self::config(&dir, &clock, gate.clone());
        let key = Arc::new(EnclaveKey::test_key());
        let mut seq = Sequencer::recover(cfg, key, broadcast::channel(4096).0, withdrawals).expect("a fresh log");
        seq.activate_legacy_protocol().expect("V23 activates");
        seq.genesis(&dev_config().genesis).expect("the dev genesis applies");
        seq.trust(sample_around(clock.as_ref())).expect("trusted time");
        seq.sequence(empty_custody_frontier()).expect("the custody scan applies");
        seq.checkpoint().expect("the scan is checkpointed");
        seq.checkpoint_status(seq.last_seq(), Some(Health::Healthy)).expect("collateral reconciles");
        seq.flush().expect("the log flushes");
        Self { seq, dir, clock, gate, signed }
    }

    /// The harness's settings on `dir`, with no checkpoint on a schedule.
    fn config<'a>(
        dir: &'a std::path::Path,
        clock: &Arc<CountingClock>,
        gate: Option<mpsc::Sender<Work>>,
    ) -> SequencerConfig<'a> {
        SequencerConfig {
            clock: clock.clone(),
            approval_registry: gate.as_ref().map(|_| Default::default()),
            checkpoints: gate,
            checkpoint_every: u64::MAX,
            ..sequencer_config(dir, watch::channel(Manifest::default()).0)
        }
    }

    /// The sequencer recovered from this node's log, as a restart finds it before its first trusted time
    /// sample: it stamps nothing until one arrives.
    fn restart(self) -> Self {
        let Self { seq, dir, clock, gate, .. } = self;
        let key = seq.key.clone();
        drop(seq);
        let (withdrawals, signed) = mpsc::unbounded_channel();
        let cfg = Self::config(&dir, &clock, gate.clone());
        let seq = Sequencer::recover(cfg, key, broadcast::channel(4096).0, withdrawals).expect("the log recovers");
        Self { seq, dir, clock, gate, signed }
    }

    /// Credits `count` accounts, so the next root has at least that many leaves to write.
    fn touch(&mut self, count: u32) {
        for index in 0..count {
            let cmd = Command::Deposit {
                account: account(index),
                amount: Fixed::from_int(10),
                chain_ref: format!("roots-{index}"),
            };
            self.seq.sequence(cmd).expect("the deposit applies").expect("trusted time admits it");
            if index % 1024 == 1023 {
                self.seq.flush().expect("the log flushes");
            }
        }
        self.seq.flush().expect("the log flushes");
    }

    /// Puts the next command's checkpoint on the schedule, a paced one, and returns its seq.
    fn pace_next(&mut self) -> u64 {
        let seq = self.seq.next_seq;
        self.seq.checkpoint_every = seq + 1;
        seq
    }

    /// Holds every one of the log's syncs from here until the test lets it pass.
    fn hold_syncs(&mut self) -> Syncs {
        let (permits, verdicts) = std::sync::mpsc::channel::<()>();
        let (entering, entered) = std::sync::mpsc::channel();
        let open = Arc::new(AtomicBool::new(false));
        let (verdicts, opened) = (Mutex::new(verdicts), open.clone());
        self.seq
            .pipeline_with_syncs(move |job| {
                _ = entering.send(());
                if !opened.load(Ordering::SeqCst) {
                    verdicts.lock().expect("one sync at a time").recv().map_err(|_| anyhow::anyhow!("no sync let"))?;
                }
                job.sync()
            })
            .expect("the sync thread starts");
        Syncs { permits, entered, open }
    }

    /// Runs the sequencer on its own thread, as the node does, with `queued` already waiting on its
    /// channel, so the first batch it takes holds them.
    fn run(self, queued: Vec<Request>) -> Running {
        let Self { seq, dir, clock, signed, .. } = self;
        let stream = seq.stream.subscribe();
        let (tx, rx) = mpsc::channel(64);
        for request in queued {
            assert!(tx.try_send(request).is_ok(), "room for the queued requests");
        }
        let (logs, logged) = mpsc::unbounded_channel();
        let (done, ended) = oneshot::channel();
        std::thread::Builder::new()
            .name("roots-test-sequencer".into())
            .spawn(move || {
                keep_callsites_interested();
                let _logging = tracing::subscriber::set_default(Registry::default().with(Recorder(logs)));
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
                let _ = done.send(runtime.block_on(seq.run(rx)));
            })
            .expect("the sequencer's thread starts");
        Running { tx, stream, logged, ended, dir, clock, signed }
    }
}

/// The log's syncs, each held until the test lets it pass.
struct Syncs {
    permits: std::sync::mpsc::Sender<()>,
    entered: std::sync::mpsc::Receiver<()>,
    open: Arc<AtomicBool>,
}

impl Syncs {
    /// Waits until a batch's sync has started and is held.
    fn started(&self) {
        self.entered.recv_timeout(WAIT).expect("a batch's sync starts");
    }

    /// Lets the held sync and every later one pass.
    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        let _ = self.permits.send(());
    }
}

/// A sequencer running on its own thread: its request channel, its stream, what it logs, and its end.
struct Running {
    tx: mpsc::Sender<Request>,
    stream: broadcast::Receiver<Arc<StreamMsg>>,
    logged: mpsc::UnboundedReceiver<Logged>,
    ended: oneshot::Receiver<anyhow::Result<()>>,
    dir: PathBuf,
    clock: Arc<CountingClock>,
    signed: mpsc::UnboundedReceiver<SignedWithdrawal>,
}

impl Running {
    /// Sends `request`.
    async fn send(&self, request: Request) {
        self.tx.send(request).await.expect("the sequencer runs");
    }

    /// Sequences `cmd` and returns its events once its batch is durable.
    async fn submit(&self, cmd: Command) -> Vec<Event> {
        let (reply, events) = oneshot::channel();
        self.send(Request::Submit { cmd, reply: Some(reply) }).await;
        timeout(WAIT, events).await.expect("the command is answered").expect("the sequencer stamps it")
    }

    /// `account`'s proof, read on the root worker.
    async fn account_proof(&self, account: AccountId) -> Option<AccountProof> {
        let (reply, proof) = oneshot::channel();
        self.send(Request::AccountProof { account, reply }).await;
        timeout(WAIT, proof).await.expect("the read is answered").expect("the worker answers")
    }

    /// The next checkpoint the stream publishes, once its batch is durable.
    async fn checkpoint(&mut self) -> Checkpoint {
        let next = async {
            loop {
                if let StreamBody::Checkpoint(checkpoint) = &self.stream.recv().await.expect("the stream runs").body {
                    return checkpoint.clone();
                }
            }
        };
        timeout(WAIT, next).await.expect("a checkpoint is published")
    }

    /// The next line the sequencer logs that `matches`, named `what` in a failure.
    async fn logged(&mut self, what: &str, matches: impl Fn(&Logged) -> bool) -> Logged {
        let next = async {
            loop {
                let logged = self.logged.recv().await.expect("the sequencer's thread logs");
                if matches(&logged) {
                    return logged;
                }
            }
        };
        timeout(WAIT, next).await.unwrap_or_else(|_| panic!("the sequencer never logged {what}"))
    }

    /// The batches the sequencer takes over 300 ms with nothing sent to it, after 200 ms for a wake-up
    /// already on its way.
    async fn batches_while_idle(&self) -> usize {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let before = self.clock.instants.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.clock.instants.load(Ordering::SeqCst) - before
    }

    /// How `run` ended, waiting for it.
    async fn ended(&mut self) -> anyhow::Result<()> {
        timeout(WAIT, &mut self.ended).await.expect("the sequencer ends").expect("it reports its end")
    }

    /// Closes the request channel, as a shutdown does, and returns how `run` ended, the log, and every
    /// line the sequencer logged that no test read.
    async fn stop(self) -> Stopped {
        let Self { tx, mut logged, mut ended, dir, .. } = self;
        drop(tx);
        let ended = timeout(WAIT, &mut ended).await.expect("the sequencer drains").expect("it reports its end");
        let mut rest = Vec::new();
        while let Some(line) = timeout(WAIT, logged.recv()).await.expect("the sequencer's thread ends") {
            rest.push(line);
        }
        Stopped { ended, entries: log_entries(&dir), logged: rest }
    }
}

/// A sequencer that has stopped: how `run` ended, its log, and the lines it logged that no test read.
struct Stopped {
    ended: anyhow::Result<()>,
    entries: Vec<LogEntry>,
    logged: Vec<Logged>,
}

/// The checkpoints in `entries`.
fn checkpoints(entries: &[LogEntry]) -> Vec<&Checkpoint> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .collect()
}
