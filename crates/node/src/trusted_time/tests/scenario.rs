//! The dev scenario offline: a sequencer under the dev genesis stamps through a test clock and is
//! checked against three loopback Roughtime servers by the trusted time task. Nothing, genesis
//! included, is stamped before the first sample. Skewing the clock past the bound halts sequencing
//! with a Critical alert, and removing the skew resumes it. With the servers silent, sequencing
//! runs on for the grace, then halts with a Critical alert until they answer again. Stamps never
//! run backwards, and nothing refused reaches the log. The API's signing clock, fed the same
//! samples, reads a time exactly while the sequencer stamps.

use std::path::PathBuf;

use engine_core::command::{Command, Sequenced};
use tokio::sync::{broadcast, oneshot, watch};
use tokio::time::sleep;

use super::*;
use crate::config::NodeConfig;
use crate::log::{LogEntry, Manifest};
use crate::sequencer::{Sequencer, SequencerConfig};
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, log_entries, sequencer_config};

/// Long enough for a silent round and a loopback round trip on a busy runner.
const WAIT: Duration = Duration::from_secs(10);
/// How far either side of the grace the scenario steps.
const SLACK: Duration = Duration::from_secs(1);
const DAY_MS: Millis = 86_400_000;
/// The start of the trusted time task's line for a round without a sample.
const FAILED_ROUND: &str = "no trusted time sample: ";

/// A sequencer under the dev genesis, and the trusted time task feeding it.
struct Node {
    tx: mpsc::Sender<Request>,
    /// How many samples the trusted time task has handed the sequencer.
    forwarded: Arc<AtomicUsize>,
    /// The clock the API signs answers with, over the samples the task publishes.
    signing: TrustedClock,
    dir: PathBuf,
    sequencer: JoinHandle<anyhow::Result<()>>,
    checker: JoinHandle<anyhow::Result<()>>,
    forwarder: JoinHandle<()>,
}

#[tokio::test]
async fn skew_and_outage_halt_sequencing_until_a_sample_within_the_bound_returns() {
    let (logs, _guard) = Logs::capture();
    let clock = Arc::new(TestClock::default());
    let servers = honest_servers(&clock).await;
    // Every reply fails to verify at first: there is no authenticated sample.
    servers.iter().for_each(|s| s.garbled.store(true, Ordering::SeqCst));
    let node = Node::start("scenario", clock.clone(), servers.iter().map(server_config).collect()).await;

    // Nothing, genesis included, is stamped before the first sample, and no answer signed.
    until(|| logs.count(FAILED_ROUND) > 0).await;
    assert_eq!(node.tick().await, None);
    assert_eq!(node.signing.now_ms(), None);
    servers.iter().for_each(|s| s.garbled.store(false, Ordering::SeqCst));
    // Past the failed queries' back-off.
    clock.advance(NO_SAMPLE_GRACE);
    node.until_forwarded(1).await;
    let first = node.tick().await.expect("a stamp within the bound");
    assert_eq!(node.signing.now_ms(), Some(clock.now_ms()));

    // 1. Skew the host clock beyond the bound: sequencing halts with one Critical alert, and the
    // API signs nothing.
    clock.skew(30_000);
    assert_eq!(node.tick().await, None);
    assert_eq!(node.tick().await, None);
    assert_eq!(logs.critical("clock_drift"), 1);
    assert_eq!(node.signing.now_ms(), None);

    // Corrected, it resumes.
    clock.skew(0);
    let resumed = node.tick().await.expect("resumed");
    assert_eq!(node.signing.now_ms(), Some(clock.now_ms()));
    assert!(resumed >= first);
    assert_eq!((logs.count("sequencing started"), logs.count("sequencing resumed")), (1, 1));

    // A host clock stepping back within the bound never moves sequenced time backwards.
    clock.skew(-1_000);
    assert!(node.tick().await.expect("within the bound") >= resumed);
    clock.skew(0);

    // 2. Block the time sources. Once a round has failed, sequencing runs on for the grace, then
    // halts.
    let failed = logs.count(FAILED_ROUND);
    servers.iter().for_each(|s| s.answering.store(false, Ordering::SeqCst));
    until(|| logs.count(FAILED_ROUND) > failed).await;
    clock.advance(NO_SAMPLE_GRACE - SLACK);
    let in_grace = node.tick().await.expect("within the grace");
    clock.advance(2 * SLACK);
    assert_eq!(node.tick().await, None);
    assert_eq!(logs.critical("trusted_time_unavailable"), 1);
    assert_eq!(node.signing.now_ms(), None);

    // The sources answer again: a fresh sample within the bound resumes sequencing.
    let forwarded = node.forwarded.load(Ordering::SeqCst);
    servers.iter().for_each(|s| s.answering.store(true, Ordering::SeqCst));
    clock.advance(NO_SAMPLE_GRACE);
    node.until_forwarded(forwarded + 1).await;
    assert!(node.tick().await.expect("resumed") >= in_grace);
    assert_eq!(logs.count("sequencing resumed"), 2);
    assert_eq!(node.signing.now_ms(), Some(clock.now_ms()));

    // The log holds genesis, checkpointed, then the accepted ticks only, in non-decreasing time.
    let entries = node.stop(true).await;
    let commands = commands(&entries);
    let stamps: Vec<Millis> = commands.iter().map(|s| s.ts).collect();
    assert!(matches!(commands[0].cmd, Command::Admin(_)), "{:?}", commands[0]);
    assert_eq!(commands.iter().filter(|s| matches!(s.cmd, Command::Tick)).count(), 5);
    assert!(stamps.is_sorted(), "{stamps:?}");
    // Genesis is checkpointed at its last command, right before the first tick.
    let last_genesis = commands.iter().take_while(|s| !matches!(s.cmd, Command::Tick)).last().expect("genesis").seq;
    let first_tick =
        entries.iter().position(|e| matches!(e, LogEntry::Command(c) if matches!(c.sequenced.cmd, Command::Tick)));
    let before_tick = &entries[first_tick.expect("a tick") - 1];
    assert!(matches!(before_tick, LogEntry::Checkpoint(cp) if cp.seq == last_genesis), "{before_tick:?}");
}

#[tokio::test]
async fn with_every_source_unreachable_nothing_is_stamped_and_the_grace_ends_in_an_alert() {
    let (logs, _guard) = Logs::capture();
    let clock = Arc::new(TestClock::default());
    let unreachable = [closed_port().await, closed_port().await, closed_port().await];
    let configs = unreachable
        .iter()
        .enumerate()
        .map(|(i, address)| RoughtimeServer {
            name: format!("unreachable-{i}"),
            address: address.clone(),
            public_key: TestServer::new(i as u8, VERSION_1).public_key(),
        })
        .collect();
    let node = Node::start("unreachable", clock.clone(), configs).await;

    assert_eq!(node.tick().await, None);
    assert_eq!(logs.critical("trusted_time_unavailable"), 0);
    clock.advance(NO_SAMPLE_GRACE + SLACK);
    assert_eq!(node.tick().await, None);
    assert_eq!(logs.critical("trusted_time_unavailable"), 1);
    assert_eq!(node.forwarded.load(Ordering::SeqCst), 0, "no sample from unreachable servers");
    assert!(commands(&node.stop(true).await).is_empty(), "nothing, genesis included, was logged");
}

#[tokio::test]
async fn a_restart_with_the_clock_a_month_ahead_stamps_nothing_until_it_is_corrected() {
    let (logs, _guard) = Logs::capture();
    let clock = Arc::new(TestClock::default());
    let servers = honest_servers(&clock).await;
    let configs = || servers.iter().map(server_config).collect::<Vec<_>>();
    let node = Node::start("restart", clock.clone(), configs()).await;
    node.until_forwarded(1).await;
    let before = node.tick().await.expect("stamped");
    node.stop(false).await;

    // The host restarts the node with its clock 30 days ahead.
    clock.skew(30 * DAY_MS);
    let node = Node::open("restart", clock.clone(), configs(), false).await;
    assert_eq!(node.tick().await, None, "nothing before the first sample, or beyond the bound");
    node.until_forwarded(1).await;
    assert_eq!(node.tick().await, None, "a clock beyond the bound");
    assert_eq!(logs.critical("clock_drift"), 1);

    clock.skew(0);
    let after = node.tick().await.expect("corrected");
    let entries = node.stop(true).await;
    assert!(commands(&entries).iter().all(|s| s.ts <= after && after < before + DAY_MS), "{before} {after}");
}

#[tokio::test]
async fn a_genesis_held_back_by_a_skewed_clock_goes_in_ahead_of_the_first_admitted_command() {
    let (logs, _guard) = Logs::capture();
    let clock = Arc::new(TestClock::default());
    let servers = honest_servers(&clock).await;
    clock.skew(30_000);
    let mut node = Node::start("held-genesis", clock.clone(), servers.iter().map(server_config).collect()).await;
    // The first sample finds the host clock beyond the bound, so genesis stays queued; no other
    // sample follows.
    node.until_forwarded(1).await;
    node.stop_feeding().await;
    assert_eq!(node.tick().await, None);
    assert_eq!(logs.critical("clock_drift"), 1);

    clock.skew(0);
    node.tick().await.expect("admitted");
    let entries = node.stop(true).await;
    let commands = commands(&entries);
    let (genesis, tick) = commands.split_at(commands.len() - 1);
    assert!(genesis.iter().all(|s| matches!(s.cmd, Command::Admin(_) | Command::HouseCapital { .. })));
    assert!(matches!(tick[0].cmd, Command::Tick), "{:?}", tick[0]);
    let at_tick = entries.iter().position(|e| matches!(e, LogEntry::Command(c) if c.sequenced.seq == tick[0].seq));
    let before_tick = &entries[at_tick.expect("the tick") - 1];
    let last_genesis = genesis.last().expect("genesis").seq;
    assert!(matches!(before_tick, LogEntry::Checkpoint(cp) if cp.seq == last_genesis), "{before_tick:?}");
}

// ── Helpers ──

impl Node {
    /// `open` on a fresh log.
    async fn start(name: &str, clock: Arc<TestClock>, servers: Vec<RoughtimeServer>) -> Self {
        Self::open(name, clock, servers, true).await
    }

    /// `boot` under the dev config, checked against `servers`.
    async fn open(name: &str, clock: Arc<TestClock>, servers: Vec<RoughtimeServer>, fresh: bool) -> Self {
        Self::boot(name, clock, scenario_config(servers), fresh).await
    }

    /// Recovers a sequencer from the log directory named after `name`, emptied first when `fresh`,
    /// queues `cfg`'s genesis if the log is empty, and starts the trusted time task, whose samples
    /// are handed to the sequencer as they come.
    async fn boot(name: &str, clock: Arc<TestClock>, cfg: NodeConfig, fresh: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("engine-trusted-time-{}-{name}", std::process::id()));
        if fresh {
            let _ = std::fs::remove_dir_all(&dir);
        }
        let (stream, _) = broadcast::channel(1024);
        let (withdrawals, _) = mpsc::unbounded_channel();
        let seq_cfg = SequencerConfig {
            clock: clock.clone(),
            max_drift: cfg.time.max_drift,
            ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
        };
        let mut seq = Sequencer::recover(seq_cfg, Arc::new(EnclaveKey::test_key()), stream, withdrawals).unwrap();
        // As the node does: both wait for a stamp trusted time admits.
        seq.activate_protocol().unwrap();
        if seq.needs_genesis() {
            seq.genesis(&cfg.genesis).unwrap();
        }
        let (tx, rx) = mpsc::channel(1024);
        let sequencer = tokio::spawn(seq.run(rx));
        let (sample_tx, mut samples) = mpsc::channel(1);
        let (published, latest) = watch::channel(None);
        let signing = TrustedClock::new(clock.clone(), latest, cfg.time.max_drift);
        let checker = tokio::spawn(run(cfg.time, clock, sample_tx, published));
        let forwarded = Arc::new(AtomicUsize::new(0));
        let (count, to_sequencer) = (forwarded.clone(), tx.clone());
        let forwarder = tokio::spawn(async move {
            while let Some(sample) = samples.recv().await {
                if to_sequencer.send(sample).await.is_err() {
                    return;
                }
                count.fetch_add(1, Ordering::SeqCst);
            }
        });
        Self { tx, forwarded, signing, dir, sequencer, checker, forwarder }
    }

    /// Waits until the sequencer has been handed `samples` samples in all.
    async fn until_forwarded(&self, samples: usize) {
        until(|| self.forwarded.load(Ordering::SeqCst) >= samples).await;
    }

    /// Submits a tick and returns the engine's time once it is sequenced, or `None` when the
    /// sequencer refuses it.
    async fn tick(&self) -> Option<Millis> {
        let (reply, events) = oneshot::channel();
        self.tx.send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).await.unwrap();
        events.await.ok()?;
        let (reply, now) = oneshot::channel();
        self.tx.send(Request::Query(Box::new(move |engine| reply.send(engine.now()).unwrap()))).await.unwrap();
        Some(now.await.unwrap())
    }

    /// Stops the trusted time task and its forwarder, so no more samples arrive and only the
    /// node's own sender is left.
    async fn stop_feeding(&mut self) {
        self.checker.abort();
        if !self.forwarder.is_finished() {
            self.forwarder.abort();
            let _ = (&mut self.forwarder).await;
        }
    }

    /// Stops the node and returns its log, removing the log when `remove_log`.
    async fn stop(mut self, remove_log: bool) -> Vec<LogEntry> {
        self.stop_feeding().await;
        // The sequencer drains once its last sender is gone.
        drop(self.tx);
        timeout(WAIT, self.sequencer).await.expect("sequencer drained").unwrap().unwrap();
        let entries = log_entries(&self.dir);
        if remove_log {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
        entries
    }
}

/// The dev config, checked against `servers`. Loopback replies have a radius of a second or two,
/// so a 3s bound fits them and keeps a silent round short.
fn scenario_config(servers: Vec<RoughtimeServer>) -> NodeConfig {
    let mut cfg = dev_config();
    cfg.time.max_drift = Duration::from_secs(3);
    cfg.time.interval = Duration::from_millis(20);
    cfg.time.servers = servers;
    cfg
}

/// The commands in `entries`.
fn commands(entries: &[LogEntry]) -> Vec<&Sequenced> {
    entries
        .iter()
        .filter_map(|e| match e {
            LogEntry::Command(c) => Some(&c.sequenced),
            LogEntry::Checkpoint(_) | LogEntry::Withdrawal(_) => None,
        })
        .collect()
}

/// Waits until `done` holds, while the node's tasks run.
async fn until(done: impl Fn() -> bool) {
    timeout(WAIT, async {
        while !done() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the scenario reached its next step in time");
}
