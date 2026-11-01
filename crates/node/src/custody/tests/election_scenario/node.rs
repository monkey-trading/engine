//! The election scenario's node: the real sequencer on a test clock, the custody watcher over the
//! three-proxy quorum mirroring the staking contract and the registry, and the API.

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
use engine_core::command::AdminCommand;
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
use crate::api::{ApiState, MAX_BACKFILLS, router};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::config::{CustodyProvider, CustodySection, FinalityTag, Genesis, RateLimit, RpcProviderSection};
use crate::custody::Custody;
use crate::history::{INDEX_FILE, Reader};
use crate::limits::ApiLimits;
use crate::log::Manifest;
use crate::sequencer::{Request, Sequencer, SequencerConfig, genesis_commands};
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{dev_config, sample_around, scratch_dir, sequencer_config};

/// The running node the scenario drives.
pub(super) struct Node {
    pub(super) state: ApiState,
    pub(super) clock: Arc<TestClock>,
    pub(super) dir: PathBuf,
    sequencer: JoinHandle<anyhow::Result<()>>,
    custody: JoinHandle<anyhow::Result<()>>,
}

impl Node {
    /// Starts the sequencer on a fresh log at `T0_SECS`, with the operator `deployer` signs as and
    /// the staking config the registry's staking contract was deployed with in its genesis, and the
    /// custody watcher reading the chain through its proxies.
    pub(super) async fn start(chain: &Chain, key: Arc<EnclaveKey>) -> Self {
        let dir = scratch_dir("election-anvil");
        let clock = Arc::new(TestClock::at(T0_SECS as Millis * 1000));
        let mut cfg = dev_config();
        cfg.custody.provider = CustodyProvider::Evm;
        cfg.custody.vault = format!("{:#x}", chain.vault);
        let operator = Operator { vault: AccountId(chain.vault.into()), ..cfg.operator().unwrap() };
        let custody = custody_section(chain, &dir, cfg.custody);
        let mut genesis: Genesis = cfg.genesis;
        genesis.house_capital = engine_core::Fixed::ZERO;
        genesis.staking = Some(StakingConfig {
            liquidity_floor: engine_core::Fixed::from_int(1_000_000),
            epoch_ms: DISTRIBUTION_EPOCH_SECS as Millis * 1000,
            start_ms: clock.now_ms(),
            cooldown_ms: COOLDOWN_SECS as Millis * 1000,
        });
        let (stream, _) = broadcast::channel(1024);
        let (withdrawals, signed) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let auth = ClientAuth::Eip712 { chain_id: 31337, vault: operator.vault };
        let seq_cfg = SequencerConfig {
            vault: operator.vault,
            operator,
            client_auth: auth.clone(),
            clock: clock.clone(),
            genesis: genesis_commands(&genesis),
            ..sequencer_config(&dir, manifest_tx)
        };
        let mut seq = Sequencer::recover(seq_cfg, key.clone(), stream.clone(), withdrawals).unwrap();
        let custody = Custody::connect(&custody, seq.last_seq()).unwrap();
        seq.activate_protocol().unwrap();
        seq.genesis(&genesis).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        let (tx, rx) = mpsc::channel(1024);
        let sequencer = tokio::spawn(seq.run(rx));
        let custody = tokio::spawn(custody.run(signed, tx.clone()));
        let bundle = AttestationBundle {
            enclave_address: key.address(),
            config_sha256: [0; 32],
            tls_public_key_sha256: [0; 32],
        };
        let unlimited = |_: RateLimit| Quota::per_second(NonZeroU32::MAX);
        let state = ApiState {
            tx,
            stream,
            attestation: Arc::new(attest_bundle(&MockAttestor, &bundle).unwrap()),
            auth,
            protocol: engine_core::protocol::ProtocolVersion::LATEST,
            access: None,
            operator,
            mock_custody: false,
            clock: Arc::new(TestClock::at(T0_SECS as Millis * 1000)),
            log_dir: dir.clone(),
            manifest,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: Arc::new(ApiLimits::with_quota(&dev_config().api, unlimited, DefaultClock::default())),
            history: Reader::new(dir.join(INDEX_FILE), watch::channel(true).1),
            marks: crate::test_support::marks_reader(),
            emission_budget: None,
            payouts_enabled: false,
            collateral: None,
            listing: Default::default(),
            signing: crate::test_support::signing(),
        };
        Node { state, clock, dir, sequencer, custody }
    }

    /// Moves the sequenced time to `ms`, trusted time with it, and the API's clock to `api`.
    pub(super) async fn advance_to(&mut self, ms: Millis, api: fn() -> Millis) {
        let by = u64::try_from(ms - self.clock.now_ms()).expect("time only moves forward");
        self.clock.advance(Duration::from_millis(by));
        self.state.tx.send(Request::TimeSample(sample_around(self.clock.as_ref()))).await.unwrap();
        self.state.clock = Arc::new(crate::clock::test_clock::TestClock::at(api()));
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

    /// Stops the custody watcher and drains the sequencer.
    pub(super) async fn stop(self) {
        self.custody.abort();
        let _ = self.custody.await;
        drop(self.state);
        self.sequencer.await.unwrap().unwrap();
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

/// EVM custody over the chain's proxies at the latest block, with the staking contract and the
/// registry mirrored.
fn custody_section(chain: &Chain, dir: &std::path::Path, mut custody: CustodySection) -> CustodySection {
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
    evm.withdrawal_finality_tag = FinalityTag::Latest;
    evm.confirmations = 0;
    evm.start_block = 0;
    evm.poll_interval = Duration::from_millis(30);
    evm.cursor_path = dir.join("custody.json");
    evm.staking = Some(
        toml::from_str(&format!(
            "contract = '{:#x}'\ncursor_path = '{}'\n[registry]\ncontract = '{:#x}'",
            chain.staking,
            dir.join("staking.json").display(),
            chain.registry
        ))
        .unwrap(),
    );
    custody.chain_id = 31337;
    custody.evm = Some(evm);
    custody
}
