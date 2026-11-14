//! The election scenarios' node: the real sequencer on a test clock, the custody watcher over the
//! three-proxy quorum mirroring the staking contract and the registry, the API, and, for the
//! operator's share, the checkpoint worker posting roots to the vault. A daily turn's member runs
//! as a seat: its own operator key and the release its sealed keys run, with the sequencer's
//! stream followed for the head a handover hands over.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engine_core::Engine;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::Command;
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::types::{AccountId, Millis};
use governor::Quota;
use governor::clock::DefaultClock;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use super::T0_SECS;
use super::chain::Chain;
use crate::api::Signing;
use crate::api::{ApiState, Feed, MAX_BACKFILLS, router};
use crate::checkpoints::staking_scenario::access::Source;
use crate::checkpoints::{CheckpointChain, View};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::config::{CustodyProvider, RateLimit};
use crate::custody::{Custody, OperatorsReader};
use crate::handover::Published;
use crate::limits::ApiLimits;
use crate::log::{CheckpointParent, Manifest};
use crate::log_check::Due;
use crate::metrics::Metrics;
use crate::node::member::Replica;
use crate::sequencer::{LogCheckSetup, Request, Sequencer, SequencerConfig, genesis_commands};
use crate::tee::sealing::KeyStore;
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{
    CHECKPOINT_EVERY, SEGMENT_COMMANDS, dev_config, sample_around, scratch_dir, sequencer_config,
};
use crate::trusted_time::{Sample, TrustedClock};

/// Commands between the checkpoints a scenario that posts them signs: few enough to reach one with
/// a handful of ticks, and more than the custody watcher's progress after each post sequences, so
/// the node goes quiet between a scenario's steps.
const POSTED_CHECKPOINT_EVERY: u64 = 10;
/// Commands per log segment, and between checkpoints, on a seat, which sequences under a load of
/// orders: enough that neither sealing segments nor signing checkpoints is most of the work, for the
/// seat or for the members following it.
pub(super) const SEAT_SEGMENT_COMMANDS: u64 = 1_000;
const SEAT_CHECKPOINT_EVERY: u64 = 500;

/// The running node the scenario drives.
pub(super) struct Node {
    pub(super) state: ApiState,
    pub(super) clock: Arc<TestClock>,
    pub(super) dir: PathBuf,
    sequencer: JoinHandle<anyhow::Result<()>>,
    custody: JoinHandle<anyhow::Result<()>>,
    /// The checkpoint worker and the task relaying its status, when the scenario posts checkpoints.
    checkpoints: Option<(JoinHandle<anyhow::Result<()>>, JoinHandle<()>)>,
    /// The signed screening source deposits and orders pass through, when the scenario moves funds.
    access: Option<Source>,
    /// The operator list's reader, when the chain has a lens.
    operators: Option<JoinHandle<anyhow::Result<()>>>,
    history: JoinHandle<anyhow::Result<()>>,
    /// The trusted time sample the API's signing reads, kept current as the clock moves.
    samples: watch::Sender<Option<Sample>>,
    key: Arc<EnclaveKey>,
    posting: bool,
    /// A seat's stream since the start, whose drain gives a handover's final head, and whether the
    /// sequencer signs its checkpoints.
    pub(super) handover: Option<(Published, watch::Receiver<bool>)>,
    /// A seat's API served over TLS.
    api: Option<JoinHandle<anyhow::Result<()>>>,
    /// A seat's asking for the boundary's log check, and the checks its sequencer publishes as due.
    pub(super) asking: Option<Asking>,
    /// When a seat published its commands, and the task recording it.
    pub(super) stamps: Arc<std::sync::Mutex<Stamps>>,
    tracking: JoinHandle<()>,
}

/// What a restart keeps of the node it replaces: its log directory, clock and screening source.
type Resume = (PathBuf, Arc<TestClock>, Option<Source>);
/// A seat's asking task for the boundary's log check, and the checks its sequencer publishes as due.
type Asking = (JoinHandle<anyhow::Result<()>>, watch::Receiver<Option<Due>>);
/// A seat, the replica it starts from, if any, its metrics and its sealed keys.
type Seated = (Seat, Option<Replica>, Arc<Metrics>, Arc<KeyStore>);

/// A daily turn's member: the operator key its config names, the release its sealed keys run, which
/// takes each turn the log assigns that key, and where it serves its API over TLS, as its registered
/// hostname names it, for the members that follow it.
#[derive(Clone, Copy)]
pub(super) struct Seat {
    pub(super) operator: AccountId,
    pub(super) release: B256,
    pub(super) api: SocketAddr,
    /// The account its checkpoint posts are sent from, its own, so no member's pending post holds
    /// another's.
    pub(super) relayer: alloy::primitives::Address,
}

impl Node {
    /// Starts the sequencer on a fresh log at `T0_SECS`, with the operator `deployer` signs as and
    /// the staking config the registry's staking contract was deployed with in its genesis, and the
    /// custody watcher reading the chain through its proxies.
    pub(super) async fn start(chain: &Chain, key: Arc<EnclaveKey>) -> Self {
        Self::launch(chain, key, false, None, None).await
    }

    /// Starts `seat`'s node over the log in `dir` (a fresh log when it holds none) on `clock`, with
    /// the screening `source` deposits and orders pass through, if any, its replay counted in
    /// `metrics`, posting checkpoints, and keeping its log check with its sealed keys in `store`.
    pub(super) async fn start_seated(
        chain: &Chain,
        key: Arc<EnclaveKey>,
        (seat, metrics, store): (Seat, Arc<Metrics>, Arc<KeyStore>),
        dir: PathBuf,
        clock: Arc<TestClock>,
        source: Option<Source>,
    ) -> Self {
        Self::launch(chain, key, true, Some((dir, clock, source)), Some((seat, None, metrics, store))).await
    }

    /// Starts `seat`'s node from the replica a member's tail handover left, over its copy in `dir`,
    /// on `clock`, with `source` as `start_seated` takes it: nothing replays the log, which `metrics`,
    /// the member's own, would count.
    pub(super) async fn start_from_replica(
        chain: &Chain,
        key: Arc<EnclaveKey>,
        (seat, metrics, store): (Seat, Arc<Metrics>, Arc<KeyStore>),
        (dir, clock): (PathBuf, Arc<TestClock>),
        replica: Replica,
        source: Option<Source>,
    ) -> Self {
        let seated = Some((seat, Some(replica), metrics, store));
        Self::launch(chain, key, true, Some((dir, clock, source)), seated).await
    }

    /// `start`, with the checkpoint worker posting roots to the vault and a signed screening source,
    /// for a scenario that moves funds.
    pub(super) async fn start_posting_checkpoints(chain: &Chain, key: Arc<EnclaveKey>) -> Self {
        Self::launch(chain, key, true, None, None).await
    }

    /// Stops this node and starts another over the same log, clock and screening source, recovering
    /// as a boot does: from the vault's posted checkpoint.
    pub(super) async fn restart(self, chain: &Chain) -> Self {
        let (key, posting, dir, clock) = (self.key.clone(), self.posting, self.dir.clone(), self.clock.clone());
        let access = self.stop().await;
        Self::launch(chain, key, posting, Some((dir, clock, access)), None).await
    }

    async fn launch(
        chain: &Chain,
        key: Arc<EnclaveKey>,
        post_checkpoints: bool,
        resume: Option<Resume>,
        seated: Option<Seated>,
    ) -> Self {
        let (seat, replica, metrics, store) = match seated {
            Some((seat, replica, metrics, store)) => (Some(seat), replica, metrics, Some(store)),
            None => (None, None, Arc::new(Metrics::new()), None),
        };
        let (dir, clock, resumed_source) = match resume {
            Some((dir, clock, source)) => (dir, clock, Some(source)),
            None => (
                scratch_dir(if post_checkpoints { "operator-share-anvil" } else { "election-anvil" }),
                Arc::new(TestClock::at(T0_SECS as Millis * 1000)),
                None,
            ),
        };
        let mut cfg = dev_config();
        cfg.custody.provider = CustodyProvider::Evm;
        cfg.custody.vault = format!("{:#x}", chain.vault);
        let mut operator = Operator { vault: AccountId(chain.vault.into()), ..cfg.operator().unwrap() };
        if let Some(seat) = seat {
            operator.address = seat.operator;
        }
        let custody_cfg = custody_section(chain, &dir, cfg.custody);
        let genesis = setup::scenario_genesis(cfg.genesis);
        let (stream, _) = broadcast::channel(1024);
        let (stamps, tracking) = stamps::track(&stream);
        let (withdrawals, signed) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let auth = ClientAuth::Eip712 { chain_id: 31337, vault: operator.vault };
        let (work_tx, work_rx) = mpsc::channel(64);
        let source = match (resumed_source, post_checkpoints) {
            (Some(source), _) => source,
            (None, true) => Some(Source::start(clock.clone(), operator).await),
            (None, false) => None,
        };
        let relayer = seat.map_or(chain.accounts[9], |seat| seat.relayer);
        let poster = post_checkpoints.then(|| setup::poster(chain, &dir, &key, relayer));
        let (due, asked) = watch::channel(None);
        let checkpoint_parent = match &poster {
            Some(poster) => match poster.chain.state(View::Finalized).await.unwrap().checkpoint {
                Some((seq, root)) => CheckpointParent { has_checkpoint: true, seq, root: root.parse().unwrap() },
                None => CheckpointParent::default(),
            },
            None => CheckpointParent::default(),
        };
        let mut seq_cfg = SequencerConfig {
            vault: operator.vault,
            operator,
            client_auth: auth.clone(),
            clock: clock.clone(),
            genesis: genesis_commands(&genesis),
            checkpoint_every: match (post_checkpoints, seat) {
                (_, Some(_)) => SEAT_CHECKPOINT_EVERY,
                (true, None) => POSTED_CHECKPOINT_EVERY,
                (false, None) => CHECKPOINT_EVERY,
            },
            segment_commands: if seat.is_some() { SEAT_SEGMENT_COMMANDS } else { SEGMENT_COMMANDS },
            checkpoint_parent,
            checkpoints: post_checkpoints.then_some(work_tx),
            access_policy: source.as_ref().map(|source| source.policy.clone()),
            turn_release: seat.map(|seat| seat.release),
            log_check: seat.zip(store.clone()).map(|(seat, store)| LogCheckSetup { release: seat.release, store, due }),
            governance: setup::governance(chain),
            metrics: metrics.clone(),
            ..sequencer_config(&dir, manifest_tx)
        };
        let signing = seat.map(|_| {
            let (signing_tx, signing) = watch::channel(true);
            seq_cfg.checkpoint_signing = Some(signing_tx);
            signing
        });
        // As a boot does: the replay feeds the dashboard record and checks the history index; a
        // member's replica brings both, and nothing replays.
        let mut history_cfg = dev_config();
        history_cfg.node.log_dir = dir.clone();
        history_cfg.history.dir = dir.join("history");
        history_cfg.custody.chain_id = 31337;
        history_cfg.custody.vault = format!("{:#x}", chain.vault);
        let (replayed, plan, stats) = match replica {
            Some(replica) => {
                // A posting seat anchors at the vault's checkpoint, as a member's start does (`ready`).
                let parent = seq_cfg.checkpoints.as_ref().map(|_| seq_cfg.checkpoint_parent);
                let replayed = replica.follower.into_log(parent, AccountId(key.address()), None).unwrap();
                (replayed, replica.history, replica.stats)
            }
            None => {
                let stats = crate::test_support::stats();
                let parent = seq_cfg.checkpoints.as_ref().map(|_| seq_cfg.checkpoint_parent);
                let (replayed, plan) =
                    crate::node::tests::read_log_and_history_recording(&history_cfg, &key, parent, &stats)
                        .await
                        .unwrap();
                // Counted as a boot counts its full replay (`node::recovery`).
                metrics.full_replays.inc();
                (replayed, plan, stats)
            }
        };
        // A start after a handover records its check as a node's does (`node::log_check`).
        crate::node::log_check::after_handover(store.as_deref(), operator.address, &replayed).unwrap();
        let (replayed, mut history) = plan.finish_with_log(replayed, key.clone()).unwrap();
        seq_cfg.history_sealing = history.sealing();
        let handover = signing.map(|signing| {
            (Published::new(stream.subscribe(), replayed.last_seq(), replayed.last_checkpoint()), signing)
        });
        let mut seq = Sequencer::recover_from(replayed, seq_cfg, key.clone(), stream.clone(), withdrawals).unwrap();
        seq.track_stats(stats.clone());
        let access = source.as_ref().map(|source| source.client.clone());
        let custody = Custody::connect(&custody_cfg, seq.last_seq()).unwrap().with_access(access.clone());
        seq.activate_protocol().unwrap();
        if seq.needs_genesis() {
            seq.genesis(&genesis).unwrap();
        }
        seq.trust(sample_around(clock.as_ref())).unwrap();
        history.catch_up_to(seq.last_seq());
        let feed = Feed::new(
            stream.subscribe(),
            dir.clone(),
            manifest.clone(),
            Arc::new(Semaphore::new(MAX_BACKFILLS)),
            Some(history.resume_at()),
        );
        let history_reader = history.reader();
        let history_task = tokio::spawn(history.run(feed));
        let (tx, rx) = mpsc::channel(1024);
        let recovery = seq.take_recovery_checkpoints();
        let sequencer = tokio::spawn(seq.run(rx));
        let custody = tokio::spawn(custody.run(signed, tx.clone()));
        let checkpoints = post_checkpoints.then(|| {
            let (status_tx, mut status_rx) = mpsc::channel(64);
            let sequencer_tx = tx.clone();
            let status = tokio::spawn(async move {
                while let Some(message) = status_rx.recv().await {
                    if sequencer_tx.send(message).await.is_err() {
                        break;
                    }
                }
            });
            let poster = poster.expect("a posting node builds its poster");
            (tokio::spawn(poster.run_with_recovery(work_rx, status_tx, recovery)), status)
        });
        let api_cfg = {
            let mut api = dev_config().api;
            api.operators_interval = Duration::from_millis(300);
            api
        };
        let (operators, feed) = match OperatorsReader::connect(&custody_cfg, &api_cfg).unwrap() {
            Some((reader, feed)) => (Some(tokio::spawn(reader.run())), Some(feed)),
            None => (None, None),
        };
        let bundle = AttestationBundle {
            enclave_address: key.address(),
            config_sha256: [0; 32],
            tls_public_key_sha256: [0; 32],
        };
        let (samples, samples_rx) = watch::channel(Some(sample_around(clock.as_ref())));
        let trusted = TrustedClock::new(clock.clone(), samples_rx.clone(), dev_config().time.max_drift);
        let asking = store.map(|store| {
            (tokio::spawn(setup::asker(chain, &tx, asked.clone(), (&store, key.clone()), trusted).run()), asked)
        });
        let unlimited = |_: RateLimit| Quota::per_second(NonZeroU32::MAX);
        let state = ApiState {
            tx,
            stream,
            attestation: Arc::new(attest_bundle(&MockAttestor, &bundle).unwrap()),
            auth,
            protocol: engine_core::protocol::ProtocolVersion::LATEST,
            access,
            operator,
            mock_custody: false,
            clock: Arc::new(TestClock::at(clock.now_ms())),
            log_dir: dir.clone(),
            manifest,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: Arc::new(ApiLimits::with_quota(&dev_config().api, unlimited, DefaultClock::default())),
            history: history_reader,
            marks: crate::test_support::marks_reader(),
            stats,
            emission_budget: None,
            payouts_enabled: post_checkpoints,
            collateral: None,
            listing: Default::default(),
            operators: feed,
            operator_hostname: None,
            signing: Arc::new(Signing::new(
                key.clone(),
                31337,
                operator.vault,
                Duration::from_secs(5),
                TrustedClock::new(clock.clone(), samples_rx, dev_config().time.max_drift),
            )),
        };
        // A seat serves its API over TLS at its registered hostname, for the members following it.
        let api = match seat {
            Some(seat) => {
                let listener = tokio::net::TcpListener::bind(seat.api).await.unwrap();
                let tls = crate::tls::TlsIdentity::generate().unwrap();
                let (app, limits) = (router(state.clone()), state.limits.clone());
                Some(tokio::spawn(crate::tls::serve(listener, tls.acceptors, app, limits, api_cfg)))
            }
            None => None,
        };
        Node {
            api,
            asking,
            stamps,
            tracking,
            state,
            clock,
            dir,
            sequencer,
            custody,
            checkpoints,
            access: source,
            operators,
            history: history_task,
            samples,
            key,
            posting: post_checkpoints,
            handover,
        }
    }

    /// Trusted time as this node's samples bound it.
    pub(super) fn trusted(&self) -> TrustedClock {
        TrustedClock::new(self.clock.clone(), self.samples.subscribe(), dev_config().time.max_drift)
    }

    /// Moves the sequenced time to `ms`, and trusted time and the API's clock with it.
    pub(super) async fn advance_to(&mut self, ms: Millis) {
        let by = u64::try_from(ms - self.clock.now_ms()).expect("time only moves forward");
        self.clock.advance(Duration::from_millis(by));
        self.samples.send_replace(Some(sample_around(self.clock.as_ref())));
        self.state.tx.send(Request::TimeSample(sample_around(self.clock.as_ref()))).await.unwrap();
        self.state.clock = Arc::new(crate::clock::test_clock::TestClock::at(ms));
    }

    /// Sequences `cmd` and returns its events.
    pub(super) async fn submit(&self, cmd: Command) -> Vec<Event> {
        let (reply, rx) = oneshot::channel();
        self.state.tx.send(Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
        crate::test_support::wait_for("the sequencer to answer a submission", rx)
            .await
            .expect("trusted time admits the command")
    }

    /// Reads the engine between batches.
    pub(super) async fn query<T: Send + 'static>(&self, read: impl FnOnce(&Engine) -> T + Send + 'static) -> T {
        let (reply, rx) = oneshot::channel();
        let query = Box::new(move |e: &Engine| {
            let _ = reply.send(read(e));
        });
        self.state.tx.send(Request::Query(query)).await.unwrap();
        rx.await.unwrap()
    }

    /// Polls the engine until `done` holds, failing if the custody watcher or the sequencer ends.
    pub(super) async fn wait(&self, what: &str, done: impl Fn(&Engine) -> bool + Clone + Send + 'static) {
        crate::test_support::wait_for(what, async {
            loop {
                assert!(!self.custody.is_finished() && !self.sequencer.is_finished(), "a node task ended");
                let check = done.clone();
                if self.query(move |engine| check(engine)).await {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        })
        .await;
    }

    /// Stops the checkpoint worker, the operator list's reader and the custody watcher, and drains the
    /// sequencer. Returns the screening source, for a node that resumes.
    pub(super) async fn stop(self) -> Option<Source> {
        if let Some(api) = self.api {
            api.abort();
            let _ = api.await;
        }
        if let Some((asking, _)) = self.asking {
            asking.abort();
            let _ = asking.await;
        }
        if let Some(operators) = self.operators {
            operators.abort();
            let _ = operators.await;
        }
        if let Some((poster, status)) = self.checkpoints {
            poster.abort();
            let _ = poster.await;
            status.abort();
            let _ = status.await;
        }
        self.custody.abort();
        let _ = self.custody.await;
        drop(self.state);
        self.sequencer.await.unwrap().unwrap();
        self.history.abort();
        let _ = self.history.await;
        self.tracking.abort();
        self.access
    }
}

mod http;
mod setup;
pub(super) use setup::custody_section;
mod stamps;
pub(super) use stamps::Stamps;
