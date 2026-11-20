//! The log generator: launch flow sequenced into a node's log on accelerated sequenced time, as
//! `wait_bench`'s rig drives a sequencer. A test clock steps from one due command to the next, so a
//! day of flow takes as long as its commands take to sequence. Each second carries the template's
//! four ticks, each market's price commands at `load_scenario`'s live rates while its venue is open,
//! and the load generator's request mix at a multiple of launch flow, from the book's named traders,
//! each signed and screened as the API forwards it, and the staking mirror's finalised frontier each
//! minute, which a staking epoch closes behind. The operator publishes each day's benchmark
//! rates before its rollover. A checkpoint is written every `CHECKPOINT_EVERY` commands, as the
//! template sets, and reported final at once, as a chain that keeps up would.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::B256;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::types::{AccountId, MarketId, Millis};
use tokio::sync::{broadcast, mpsc, watch};

use crate::checkpoints::{Health, Registry, Work};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::common::close_spread::source_key;
use crate::common::launch_book::{benchmark_batch_on, price_command_on};
use crate::common::session_keys::HOUR_MS;
use crate::common::{CHAIN_ID, Client, VAULT, operator};
use crate::log::{Checkpoint, Manifest};
use crate::sequencer::recovery_scale::orders::Traders;
use crate::sequencer::{ReplayedLog, Sequencer, SequencerConfig, StreamMsg};
use crate::snapshot::sealing::Endpoint;
use crate::tee::EnclaveKey;
use crate::test_support::{sample_around, sequencer_config, stats};

/// The template's `[node]` cadences (`ops/runbooks/production-node.toml`): a checkpoint and a
/// segment every 1,000 commands, and a tick every 250 ms.
pub(super) const CHECKPOINT_EVERY: u64 = 1_000;
pub(super) const SEGMENT_COMMANDS: u64 = 1_000;
const TICK_US: u64 = 250_000;
const DAY_US: u64 = 86_400_000_000;
/// How often trusted time is sampled, as the node's Roughtime client does.
const SAMPLE_US: u64 = 10_000_000;
/// Commands between the generator's flushes, each a batch's sync, after which a checkpoint written is
/// reported final.
const FLUSH_EVERY: u64 = 1_000;
/// How often the staking mirror's finalised frontier is reported, which an epoch closes behind.
const FRONTIER_US: u64 = 60_000_000;
/// Each market's mid: the book's mark, and half a basis point off it in turn, up in odd markets and
/// down in even ones, so no price reaches the book's in-band orders (`load_scenario::venues`).
const MIDS: [[&str; 2]; 2] = [["100", "99.995"], ["100", "100.005"]];

/// A node on the book's log: its sequencer on a test clock, and the checkpoint work it hands the
/// poster, each reported final.
pub(super) struct Node {
    pub(super) seq: Sequencer,
    pub(super) clock: Arc<TestClock>,
    queued: mpsc::Receiver<Work>,
    /// The log's view the writer publishes, which readers of the log read it through.
    pub(super) manifest: watch::Receiver<Manifest>,
    /// The newest checkpoint written.
    pub(super) checkpoint: Option<Checkpoint>,
    pending: u64,
}

/// The emission periods and staking epochs a node's engine has closed.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Closed {
    pub periods: u64,
    pub epochs: u64,
}

/// A node's link to its history owner, when its log pairs recovery caches: the endpoint its captures
/// go through, and the stream the owner's feed follows.
pub(super) struct Pairing {
    pub sealing: Endpoint,
    pub stream: broadcast::Sender<Arc<StreamMsg>>,
}

/// What a node's sequencer is configured with: the book's deployment, operator and screening source,
/// the template's cadences, and `sealing`, where its captures go, when its log pairs caches.
fn config<'a>(
    dir: &'a Path,
    clock: Arc<TestClock>,
    sink: mpsc::Sender<Work>,
    manifest: watch::Sender<Manifest>,
    sealing: Option<Endpoint>,
) -> SequencerConfig<'a> {
    SequencerConfig {
        clock,
        history_sealing: sealing,
        daily_snapshots: None,
        access_policy: Some(engine_core::access::AccessPolicy { source: source_key(), max_age_ms: 10 * HOUR_MS }),
        checkpoints: Some(sink),
        checkpoint_every: CHECKPOINT_EVERY,
        segment_commands: SEGMENT_COMMANDS,
        checkpoint_registry: Some(Registry::default()),
        approval_registry: Some(Default::default()),
        chain_id: CHAIN_ID,
        vault: VAULT,
        client_auth: ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT },
        operator: operator(),
        // The log holds no genesis, the book's being engine state, so the config names none.
        genesis: Vec::new(),
        close_spread_ceilings: Vec::new(),
        ..sequencer_config(dir, manifest)
    }
}

impl Node {
    /// A node that takes over `book` on `log`, the empty log read from `dir`, signing with `key`, with
    /// the stats worker whose copy encodes the caches `pairing`'s history owner publishes.
    /// Error cases:
    /// - the start checks refuse the log, or the sequencer refuses the book
    pub(super) fn start(
        book: engine_core::Engine,
        log: ReplayedLog,
        dir: &Path,
        key: Arc<EnclaveKey>,
        pairing: Pairing,
    ) -> anyhow::Result<Self> {
        let clock = Arc::new(TestClock::at(book.now() + 1_000));
        let (sink, queued) = mpsc::channel(1 << 16);
        let (published, manifest) = watch::channel(Manifest::default());
        let cfg = config(dir, clock.clone(), sink, published, Some(pairing.sealing));
        log.validate_recovery(&cfg, AccountId(key.address()))?;
        let mut seq = Sequencer::recover_from(log, cfg, key, pairing.stream, mpsc::unbounded_channel().0)?;
        // The stats worker's copy starts from the book, as a node's from the state it recovered, before
        // the first command and checkpoint, which `take_over_checkpointed` writes otherwise.
        seq.take_over(book);
        seq.track_stats(stats());
        seq.trust(sample_around(clock.as_ref()))?;
        anyhow::ensure!(seq.sequence(Command::Tick)?.is_some(), "trusted time refused the first stamp");
        seq.checkpoint()?;
        seq.flush()?;
        let mut node = Node { seq, clock, queued, manifest, checkpoint: None, pending: 0 };
        node.settle();
        Ok(node)
    }

    /// A node with no paired caches that takes over `book` with an empty log in `dir`, signing with `key`.
    pub(super) fn unpaired(book: engine_core::Engine, dir: &Path, key: Arc<EnclaveKey>) -> Self {
        Self::publishing(book, dir, key, None)
    }

    /// `unpaired`, publishing a daily snapshot into `daily` at the first checkpoint of each UTC day, as a
    /// node with `[daily_snapshots]` does.
    pub(super) fn publishing(
        book: engine_core::Engine,
        dir: &Path,
        key: Arc<EnclaveKey>,
        daily: Option<&Path>,
    ) -> Self {
        let clock = Arc::new(TestClock::at(book.now() + 1_000));
        let (sink, queued) = mpsc::channel(1 << 16);
        let (published, manifest) = watch::channel(Manifest::default());
        let cfg = SequencerConfig {
            daily_snapshots: daily.map(Path::to_owned),
            ..config(dir, clock.clone(), sink, published, None)
        };
        let mut seq =
            Sequencer::recover(cfg, key, broadcast::channel(16).0, mpsc::unbounded_channel().0).expect("an empty log");
        seq.trust(sample_around(clock.as_ref())).expect("a trusted time sample");
        seq.take_over_checkpointed(book).expect("the book's first checkpoint");
        let mut node = Node { seq, clock, queued, manifest, checkpoint: None, pending: 0 };
        node.settle();
        node
    }

    /// The node `log`, read from `dir`, recovers into, signing with `key`, as a restart does: with
    /// `pairing`, after the start checks, and with the stats worker a node with paired caches runs.
    /// Error cases:
    /// - the start checks or the sequencer refuse the log
    pub(super) fn recovered(
        log: ReplayedLog,
        dir: &Path,
        key: Arc<EnclaveKey>,
        pairing: Option<Pairing>,
    ) -> anyhow::Result<Self> {
        let at = log.replayed.engine.now() + 1_000;
        let clock = Arc::new(TestClock::at(at));
        let (sink, queued) = mpsc::channel(1 << 16);
        let (published, manifest) = watch::channel(Manifest::default());
        let (sealing, stream) = pairing.map_or((None, broadcast::channel(16).0), |p| (Some(p.sealing), p.stream));
        let paired = sealing.is_some();
        let cfg = config(dir, clock.clone(), sink, published, sealing);
        if paired {
            log.validate_recovery(&cfg, AccountId(key.address()))?;
        }
        let mut seq = Sequencer::recover_from(log, cfg, key, stream, mpsc::unbounded_channel().0)?;
        if paired {
            seq.track_stats(stats());
        }
        Ok(Node { seq, clock, queued, manifest, checkpoint: None, pending: 0 })
    }

    /// Samples trusted time and sequences a tick, the first command a restarted node stamps.
    /// Error cases:
    /// - trusted time refuses the stamp, or the log cannot be written
    pub(super) fn first_command(&mut self) -> anyhow::Result<()> {
        self.seq.trust(sample_around(self.clock.as_ref()))?;
        anyhow::ensure!(self.seq.sequence(Command::Tick)?.is_some(), "trusted time refused the first stamp");
        self.seq.flush()?;
        self.settle();
        Ok(())
    }

    /// Moves the clock to `ts`, if it is ahead.
    fn advance_to(&self, ts: Millis) {
        let ahead = ts - self.clock.now_ms();
        if ahead > 0 {
            self.clock.advance(Duration::from_millis(ahead as u64));
        }
    }

    /// The emission periods and staking epochs the engine has closed since emissions and staking started:
    /// the period and the epoch it is in, each numbered from 0 at the start. Read from the engine, since a
    /// client command whose stamp passes a close takes a `Tick` first (`Sequencer::stamp`), whose events
    /// its reply does not carry.
    pub(super) fn closed(&self) -> Closed {
        let engine = &self.seq.engine;
        Closed {
            periods: engine.emissions().map_or(0, |emissions| emissions.period),
            epochs: engine.staking().map_or(0, |staking| staking.epoch),
        }
    }

    /// Sequences `cmd` now, returning its events, and flushes every `FLUSH_EVERY` commands.
    fn sequence(&mut self, cmd: Command) -> Vec<Event> {
        let stamped = self.seq.sequence(cmd).expect("the log takes the command").expect("trusted time admits it");
        self.pending += 1;
        if self.pending >= FLUSH_EVERY {
            self.seq.flush().expect("the batch syncs");
            self.settle();
        }
        stamped.to_vec()
    }

    /// Reports the newest checkpoint written final, so the next is not held back.
    fn settle(&mut self) {
        self.pending = 0;
        if let Some(work) = std::iter::from_fn(|| self.queued.try_recv().ok()).last() {
            let seq = work.checkpoint.seq;
            self.checkpoint = Some(work.checkpoint);
            self.seq.checkpoint_status(Some(seq), Some(Health::Healthy)).expect("the gate takes the report");
        }
    }

    /// Sequences a tick and writes a checkpoint over it, as a stopping node does, and returns it, final.
    pub(super) fn checkpoint_now(&mut self) -> Checkpoint {
        self.checkpoint_with(|_, _| ()).0
    }

    /// `checkpoint_now`, running `at` on the checkpoint before it is reported final, as a capture at a
    /// checkpoint runs.
    pub(super) fn checkpoint_with<T>(&mut self, at: impl FnOnce(&Self, &Checkpoint) -> T) -> (Checkpoint, T) {
        // Stamped apart from the flushes `sequence` makes, so no report takes the checkpoint's work first.
        self.seq.sequence(Command::Tick).expect("the log takes the tick").expect("trusted time admits it");
        self.seq.checkpoint().expect("the checkpoint is written");
        self.seq.flush().expect("the batch syncs");
        let work = std::iter::from_fn(|| self.queued.try_recv().ok()).last().expect("the checkpoint's work");
        let checkpoint = work.checkpoint;
        assert_eq!(Some(checkpoint.seq), self.seq.last_seq(), "the checkpoint covers the last command");
        let done = at(self, &checkpoint);
        self.checkpoint = Some(checkpoint.clone());
        self.seq.checkpoint_status(Some(checkpoint.seq), Some(Health::Healthy)).expect("the gate takes the report");
        self.pending = 0;
        (checkpoint, done)
    }
}

/// One kind of command the flow sends on a schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Due {
    Sample,
    Tick,
    Price(MarketId),
    Request,
    Frontier,
}

/// What a stretch of flow sequenced: the price commands the engine applied and the requests among its
/// commands, the fills the requests made and the requests the engine rejected.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Sequenced {
    /// The sequenced seconds it covered.
    pub secs: u64,
    pub prices: u64,
    pub requests: u64,
    pub fills: u64,
    pub rejected: u64,
}

impl Sequenced {
    fn add(&mut self, other: Sequenced) {
        self.secs += other.secs;
        self.prices += other.prices;
        self.requests += other.requests;
        self.fills += other.fills;
        self.rejected += other.rejected;
    }
}

/// The flow: the traders, each market's price rate, and where the schedule stands, which a run takes up
/// where the last one left it, on whichever node it is given.
pub(super) struct Flow {
    traders: Traders,
    /// Each market's price commands a second while its venue is open.
    rates: BTreeMap<MarketId, f64>,
    /// Price commands each market has sent, which picks its next mid.
    moves: BTreeMap<MarketId, u64>,
    /// The UTC day whose benchmark rates were last published.
    published: Option<i64>,
    /// Each kind of command's next time, in microseconds of flow, from `origin_ms`, its first run's
    /// first stamp; and how far the flow has run.
    due: BinaryHeap<Reverse<(u64, Due)>>,
    origin_ms: Option<Millis>,
    ran_us: u64,
    /// The finalised frontier's last block.
    blocks: u64,
}

impl Flow {
    /// The flow of `traders`, each market priced at its rate in `rates`.
    pub(super) fn new(traders: Traders, rates: BTreeMap<MarketId, f64>) -> Self {
        let mut due = BinaryHeap::new();
        due.extend([Due::Sample, Due::Tick, Due::Request, Due::Frontier].map(|kind| Reverse((0, kind))));
        due.extend(rates.keys().map(|market| Reverse((0, Due::Price(*market)))));
        Flow { traders, rates, moves: BTreeMap::new(), published: None, due, origin_ms: None, ran_us: 0, blocks: 0 }
    }

    /// Sequences the next `span` of flow at `multiple` times launch flow into `node`, calling `day` with
    /// what each whole day of it sequenced and the wall time it took.
    pub(super) fn run(
        &mut self,
        node: &mut Node,
        span: Duration,
        multiple: f64,
        mut day: impl FnMut(&mut Node, Sequenced, Duration),
    ) -> Sequenced {
        let origin_ms = *self.origin_ms.get_or_insert_with(|| node.clock.now_ms());
        let (begin_us, end_us) = (self.ran_us, self.ran_us + span.as_micros() as u64);
        let (mut total, mut today) = (Sequenced::default(), Sequenced::default());
        let (mut day_end, mut day_began) = (begin_us + DAY_US, Instant::now());
        while let Some(Reverse((at_us, kind))) = self.due.pop() {
            if at_us >= end_us {
                self.due.push(Reverse((at_us, kind)));
                break;
            }
            if at_us >= day_end {
                node.seq.flush().expect("the batch syncs");
                node.settle();
                today.secs = DAY_US / 1_000_000;
                day(node, today, day_began.elapsed());
                total.add(today);
                (today, day_end, day_began) = (Sequenced::default(), day_end + DAY_US, Instant::now());
            }
            let ts = origin_ms + (at_us / 1_000) as Millis;
            node.advance_to(ts);
            let next_us = match kind {
                Due::Sample => {
                    node.seq.trust(sample_around(node.clock.as_ref())).expect("a trusted time sample");
                    at_us + SAMPLE_US
                }
                Due::Tick => {
                    self.publish_rates(node, ts);
                    node.sequence(Command::Tick);
                    at_us + TICK_US
                }
                Due::Price(market) => {
                    let rate = self.rates[&market];
                    if node.seq.engine.markets()[&market].spec.schedule.is_open(ts) {
                        let moved = self.moves.entry(market).or_default();
                        let mid = MIDS[market as usize % 2][(*moved % 2) as usize];
                        *moved += 1;
                        let events = node.sequence(price_command_on(&node.seq.engine, market, mid, ts));
                        // A price the engine refused prices nothing.
                        today.prices += u64::from(!events.iter().any(|e| matches!(e, Event::PricesRejected { .. })));
                    }
                    at_us + (1_000_000.0 / rate) as u64
                }
                Due::Frontier => {
                    self.blocks += 1;
                    let progress = Command::StakingProgress {
                        block: self.blocks,
                        block_hash: B256::repeat_byte(0x11),
                        block_time: Some(ts),
                        named_turn: None,
                    };
                    let events = node.sequence(progress);
                    assert!(events.is_empty(), "the staking mirror's finalised frontier: {events:?}");
                    at_us + FRONTIER_US
                }
                Due::Request => {
                    let sent = self.traders.send(node, ts, |node, cmd| node.sequence(cmd));
                    today.requests += 1;
                    today.fills += sent.fills;
                    today.rejected += u64::from(sent.rejected);
                    at_us + self.traders.gap_us(multiple)
                }
            };
            self.due.push(Reverse((next_us, kind)));
        }
        self.ran_us = end_us;
        node.seq.flush().expect("the batch syncs");
        node.settle();
        if today.prices + today.requests > 0 {
            today.secs = (end_us - (day_end - DAY_US)) / 1_000_000;
            day(node, today, day_began.elapsed());
            total.add(today);
        }
        total
    }

    /// The commands `secs` of flow at `multiple` times launch flow from `start_ms` sequences on
    /// average: its ticks, its requests, a benchmark publication a day, and each market's prices over
    /// the minutes its venue is open, as `engine` lists the market, and a frontier a minute.
    pub(super) fn scheduled(&self, engine: &engine_core::Engine, start_ms: Millis, secs: u64, multiple: f64) -> f64 {
        let open_secs = |market: MarketId| {
            let schedule = &engine.markets()[&market].spec.schedule;
            (0..secs / 60).filter(|minute| schedule.is_open(start_ms + *minute as Millis * 60_000)).count() as f64
                * 60.0
        };
        let prices: f64 = self.rates.iter().map(|(market, rate)| rate * open_secs(*market)).sum();
        let ticks = secs as f64 * 1_000_000.0 / TICK_US as f64;
        let requests = secs as f64 * crate::load_scenario::plan::requests_per_second(multiple);
        prices + ticks + requests + (secs / (DAY_US / 1_000_000)) as f64 + (secs / (FRONTIER_US / 1_000_000)) as f64
    }

    /// The commands a second of launch flow sequences with every market open: the most it writes.
    pub(super) fn peak_commands_per_sec(&self) -> f64 {
        let ticks = 1_000_000.0 / TICK_US as f64;
        self.rates.values().sum::<f64>() + ticks + crate::load_scenario::plan::requests_per_second(1.0)
    }

    /// Publishes the day's benchmark rates, signed by the operator, once a UTC day, before anything
    /// else that day, as the benchmark worker does before each rollover.
    fn publish_rates(&mut self, node: &mut Node, ts: Millis) {
        let day = ts.div_euclid(86_400_000);
        if self.published == Some(day) {
            return;
        }
        self.published = Some(day);
        let operator = Client { expires_at: ts + 60_000, ..Client::operator() };
        let nonce = node.seq.engine.next_request_nonce(operator.account);
        let batch: AdminCommand = benchmark_batch_on(&node.seq.engine, ts);
        let events = node.sequence(operator.admin(nonce, batch));
        assert!(events.contains(&Event::AdminApplied), "the day's benchmark rates: {events:?}");
    }
}
