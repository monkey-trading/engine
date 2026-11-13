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

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::{Extension, Router};
use engine_core::Engine;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::{AdminCommand, Command};
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::staking::StakingConfig;
use engine_core::types::{AccountId, Millis};
use governor::Quota;
use governor::clock::DefaultClock;
use k256::ecdsa::SigningKey;
use serde_json::Value;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tower::ServiceExt;

use super::chain::Chain;
use super::{COOLDOWN_SECS, DISTRIBUTION_EPOCH_SECS, T0_SECS};
use crate::api::Signing;
use crate::api::{ApiState, Feed, MAX_BACKFILLS, router};
use crate::checkpoints::staking_scenario::access::Source;
use crate::checkpoints::{CheckpointChain, CheckpointRpc, Poster, View};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::config::{
    CheckpointSection, CustodyProvider, CustodySection, FinalityTag, Genesis, RateLimit, RpcProviderSection,
};
use crate::custody::{Custody, OperatorsReader};
use crate::handover::Published;
use crate::limits::ApiLimits;
use crate::log::{CheckpointDomain, CheckpointParent, Manifest};
use crate::sequencer::{Request, Sequencer, SequencerConfig, genesis_commands};
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{CHECKPOINT_EVERY, dev_config, sample_around, scratch_dir, sequencer_config};
use crate::trusted_time::{Sample, TrustedClock};

/// Commands between the checkpoints a scenario that posts them signs: few enough to reach one with
/// a handful of ticks, and more than the custody watcher's progress after each post sequences, so
/// the node goes quiet between a scenario's steps.
const POSTED_CHECKPOINT_EVERY: u64 = 10;

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
}

/// What a restart keeps of the node it replaces: its log directory, clock and screening source.
type Resume = (PathBuf, Arc<TestClock>, Option<Source>);

/// A daily turn's member: the operator key its config names, and the release its sealed keys run,
/// which takes each turn the log assigns that key.
#[derive(Clone, Copy)]
pub(super) struct Seat {
    pub(super) operator: AccountId,
    pub(super) release: B256,
}

impl Node {
    /// Starts the sequencer on a fresh log at `T0_SECS`, with the operator `deployer` signs as and
    /// the staking config the registry's staking contract was deployed with in its genesis, and the
    /// custody watcher reading the chain through its proxies.
    pub(super) async fn start(chain: &Chain, key: Arc<EnclaveKey>) -> Self {
        Self::launch(chain, key, false, None, None).await
    }

    /// Starts `seat`'s node over the log in `dir` (a fresh log when it holds none) on `clock`.
    pub(super) async fn start_seated(
        chain: &Chain,
        key: Arc<EnclaveKey>,
        seat: Seat,
        dir: PathBuf,
        clock: Arc<TestClock>,
    ) -> Self {
        Self::launch(chain, key, false, Some((dir, clock, None)), Some(seat)).await
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
        seat: Option<Seat>,
    ) -> Self {
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
        let mut genesis: Genesis = cfg.genesis;
        genesis.house_capital = engine_core::Fixed::ZERO;
        genesis.staking = Some(StakingConfig {
            liquidity_floor: engine_core::Fixed::from_int(1_000_000),
            epoch_ms: DISTRIBUTION_EPOCH_SECS as Millis * 1000,
            start_ms: T0_SECS as Millis * 1000,
            cooldown_ms: COOLDOWN_SECS as Millis * 1000,
        });
        let (stream, _) = broadcast::channel(1024);
        let (withdrawals, signed) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let auth = ClientAuth::Eip712 { chain_id: 31337, vault: operator.vault };
        let (work_tx, work_rx) = mpsc::channel(64);
        let source = match (resumed_source, post_checkpoints) {
            (Some(source), _) => source,
            (None, true) => Some(Source::start(clock.clone(), operator).await),
            (None, false) => None,
        };
        let poster = post_checkpoints.then(|| poster(chain, &dir, &key));
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
            checkpoint_every: if post_checkpoints { POSTED_CHECKPOINT_EVERY } else { CHECKPOINT_EVERY },
            checkpoint_parent,
            checkpoints: post_checkpoints.then_some(work_tx),
            access_policy: source.as_ref().map(|source| source.policy.clone()),
            turn_release: seat.map(|seat| seat.release),
            governance: chain.vote.map(|(vote, ttl)| engine_core::governance::GovernanceConfig {
                vote: AccountId(vote.into()),
                restriction_ttl_ms: ttl,
            }),
            ..sequencer_config(&dir, manifest_tx)
        };
        let signing = seat.map(|_| {
            let (signing_tx, signing) = watch::channel(true);
            seq_cfg.checkpoint_signing = Some(signing_tx);
            signing
        });
        // As a boot does: the replay feeds the dashboard record and checks the history index.
        let stats = crate::test_support::stats();
        let mut history_cfg = dev_config();
        history_cfg.node.log_dir = dir.clone();
        history_cfg.history.dir = dir.join("history");
        history_cfg.custody.chain_id = 31337;
        history_cfg.custody.vault = format!("{:#x}", chain.vault);
        let (replayed, plan) = crate::node::tests::read_log_and_history_recording(
            &history_cfg,
            &key,
            seq_cfg.checkpoints.as_ref().map(|_| seq_cfg.checkpoint_parent),
            &stats,
        )
        .await
        .unwrap();
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
            payouts_enabled: false,
            collateral: None,
            listing: Default::default(),
            operators: feed,
            signing: Arc::new(Signing::new(
                key.clone(),
                31337,
                operator.vault,
                Duration::from_secs(5),
                TrustedClock::new(clock.clone(), samples_rx, dev_config().time.max_drift),
            )),
        };
        Node {
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

    /// Posts `command` signed by `key` with its next nonce through the admin route, returning the
    /// status and the last event.
    pub(super) async fn admin(&self, key: &SigningKey, command: AdminCommand) -> (StatusCode, Value) {
        let address = engine_core::eip712::address(key.verifying_key());
        let nonce = self.query(move |engine| engine.next_request_nonce(address)).await;
        let expires_at = self.state.clock.now_ms() + 60_000;
        let request = crate::admin::sign(key, &self.state.operator, command, nonce, expires_at);
        let body = serde_json::to_string(&request).unwrap();
        let request = HttpRequest::post("/v1/admin").header("content-type", "application/json").body(Body::from(body));
        let (status, events) = self.send(request.unwrap()).await;
        (status, events.as_array().and_then(|events| events.last()).cloned().unwrap_or(events))
    }

    /// The status and JSON body of a GET of `path`.
    pub(super) async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(HttpRequest::get(path).body(Body::empty()).unwrap()).await
    }

    /// The status, headers and body bytes of a GET of `path` under `challenge`, as a client saves them.
    pub(super) async fn get_signed(&self, path: &str, challenge: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let request =
            HttpRequest::get(path).header(engine_wire::CHALLENGE_HEADER, challenge).body(Body::empty()).unwrap();
        let app: Router =
            router(self.state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
        let response = app.oneshot(request).await.unwrap();
        let (status, headers) = (response.status(), response.headers().clone());
        (status, headers, to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec())
    }

    /// The status and JSON body of a POST of `body` to `path`.
    pub(super) async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let request =
            HttpRequest::post(path).header("content-type", "application/json").body(Body::from(body.to_string()));
        self.send(request.unwrap()).await
    }

    /// Stops the checkpoint worker, the operator list's reader and the custody watcher, and drains the
    /// sequencer. Returns the screening source, for a node that resumes.
    pub(super) async fn stop(self) -> Option<Source> {
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
        self.access
    }

    async fn send(&self, request: HttpRequest<Body>) -> (StatusCode, Value) {
        let app: Router =
            router(self.state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}

/// The checkpoint worker posting to the chain's vault at the latest block.
fn poster(chain: &Chain, dir: &std::path::Path, key: &EnclaveKey) -> Poster<CheckpointRpc> {
    let section: CheckpointSection = toml::from_str(&format!(
        "rpc_url = {:?}\nrelayer = '{:#x}'\nretry_interval = '40ms'\nrpc_timeout = '2s'",
        chain.url, chain.accounts[9]
    ))
    .unwrap();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId(chain.vault.into()) };
    let signer = AccountId(key.address());
    Poster {
        chain: CheckpointRpc::new(&section, domain, chain.usdc, FinalityTag::Latest, 0, dir, signer).unwrap(),
        registry: None,
        domain,
        signer,
        retry: section.retry_interval,
        pending_deposits_critical_after: Duration::from_secs(1),
        post_withheld: Default::default(),
        relayer_gas: None,
    }
}

/// EVM custody over the chain's proxies, crediting at the latest block and releasing at the chain's
/// release tag, with the staking contract, the registry, and any schedule mirrored.
pub(super) fn custody_section(chain: &Chain, dir: &std::path::Path, mut custody: CustodySection) -> CustodySection {
    let anvil: crate::config::NodeConfig =
        toml::from_str(&std::fs::read_to_string(crate::test_support::ANVIL_CONFIG).unwrap()).unwrap();
    let mut evm = anvil.custody.evm.unwrap();
    evm.providers = chain
        .proxies
        .iter()
        .enumerate()
        .map(|(index, url)| {
            toml::from_str::<RpcProviderSection>(&format!("name = 'scenario-{index}'\nrpc_url = '{url}'")).unwrap()
        })
        .collect();
    evm.usdc = chain.usdc;
    evm.finality_tag = FinalityTag::Latest;
    evm.withdrawal_finality_tag = chain.release_tag;
    evm.confirmations = 0;
    evm.start_block = 0;
    evm.poll_interval = Duration::from_millis(30);
    evm.cursor_path = dir.join("custody.json");
    evm.staking = Some(
        toml::from_str(&format!(
            "contract = '{:#x}'\ncursor_path = '{}'\n[registry]\ncontract = '{:#x}'{}{}{}",
            chain.staking,
            dir.join("staking.json").display(),
            chain.registry,
            chain.lens.map(|lens| format!("\nlens = '{lens:#x}'")).unwrap_or_default(),
            chain.schedule.map(|schedule| format!("\nschedule = '{schedule:#x}'")).unwrap_or_default(),
            chain
                .vote
                .map(|(vote, ttl)| format!("\n[registry.vote]\ncontract = '{vote:#x}'\nrestriction_ttl = '{ttl}ms'"))
                .unwrap_or_default()
        ))
        .unwrap(),
    );
    custody.chain_id = 31337;
    custody.evm = Some(evm);
    custody
}
