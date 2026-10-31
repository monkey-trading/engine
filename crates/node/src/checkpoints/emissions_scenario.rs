//! MONKEY emissions dev scenario on a local Anvil: the real sequencer, API routes, checkpoint worker,
//! vault, and MONKEY token, on a clock the scenario moves.

mod api;
mod funding;

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::{Command as Process, Stdio};
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, U64, U256};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::{SolCall, SolError, SolValue};
use axum::Router;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::StatusCode;
use engine_core::Engine;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::emissions::{EmissionSchedule, Emissions, PERIODS_PER_HALVING, TokenUnits};
use engine_core::event::Event;
use engine_core::merkle::EntitlementProof;
use engine_core::price::SourceQuote;
use engine_core::types::{AccountId, Millis, Side};
use governor::{Quota, clock::DefaultClock};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::api::{ApiState, MAX_BACKFILLS, router};
use crate::checkpoints::scenario::{Anvil, deploy, send};
use crate::checkpoints::{CheckpointChain, CheckpointRpc, Health, Poster, View};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::config::{CheckpointSection, FinalityTag, RateLimit};
use crate::history::{INDEX_FILE, Reader};
use crate::limits::ApiLimits;
use crate::log::{CheckpointDomain, CheckpointParent, Manifest};
use crate::sequencer::{Request, Sequencer, SequencerConfig, replay_path};
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{dev_config, sample_around, scratch_dir, sequencer_config};

/// Tue 20 Oct 2026 14:00 UTC: where the scenario's clock starts, and period 0 with it.
const NOW: Millis = 1_792_504_800_000;
/// The period budget, 1,000 MONKEY: a development value, not an approved production parameter.
const DEV_PERIOD_BUDGET: u128 = 1_000_000_000_000_000_000_000;
/// Commands between checkpoints.
const CHECKPOINT_EVERY: u64 = 10;
const ALICE: AccountId = AccountId([0xa1; 20]);
const BOB: AccountId = AccountId([0xb0; 20]);

sol! {
    function claimEmissions(address account, uint256 entitled, bytes32[] siblings) external returns (uint256);
    function monkey() external view returns (address);
    function balanceOf(address account) external view returns (uint256);
    function latestSeq() external view returns (uint64);
    function latestRoot() external view returns (bytes32);
    error NothingToClaim(uint256 entitled, uint256 claimed);
}

#[tokio::test]
#[ignore = "requires Anvil and compiled contracts; scripts/dev-monkey-emissions.sh runs it"]
async fn dev_scenario_emissions_share_fixed_budgets_halve_survive_restart_and_claim_once() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_ansi(false).with_writer(std::io::stderr).finish(),
    );
    let mut s = Scenario::start().await;
    // A schedule with another budget than the vault's cap was derived from never reaches the log.
    let mismatched = EmissionSchedule { start_ms: NOW, period_budget: TokenUnits(DEV_PERIOD_BUDGET / 2) };
    let (status, body) = s.post("/v1/admin", s.signed(AdminCommand::InitEmissions(mismatched))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let schedule = EmissionSchedule { start_ms: NOW, period_budget: TokenUnits(DEV_PERIOD_BUDGET) };
    let (status, events) = s.post("/v1/admin", s.signed(AdminCommand::InitEmissions(schedule))).await;
    assert_eq!((status, events.as_array().unwrap().last()), (StatusCode::OK, Some(&json!({"type": "admin_applied"}))));
    for account in [ALICE, BOB] {
        s.fund(Some(account), engine_core::Fixed::from_int(1_000_000)).await;
    }
    let quarter = DEV_PERIOD_BUDGET / 4;

    // 1. Losses of $100 and $300 in one period share its fixed budget 25% and 75%.
    s.losses(&[(ALICE, "1"), (BOB, "3")], "100000", "99900").await;
    s.next_period().await;
    assert_eq!(s.entitlements().await, [quarter, 3 * quarter]);

    // 2. Losses of $1,000 and $3,000: the same budget and the same shares.
    s.losses(&[(ALICE, "1"), (BOB, "3")], "100000", "99000").await;
    let events = s.next_period().await;
    assert!(events.contains(&Event::EmissionPeriodClosed {
        period: 1,
        budget: TokenUnits(DEV_PERIOD_BUDGET),
        allocated: TokenUnits(DEV_PERIOD_BUDGET),
        cancelled: TokenUnits(0),
    }));
    assert_eq!(s.entitlements().await, [2 * quarter, 6 * quarter]);

    // 3. Periods with no loss cancel their budgets: nobody is credited, then or later.
    let events = s.advance_periods(3).await;
    assert!(
        events.iter().any(|e| matches!(e, Event::EmissionPeriodsCancelled { first: 3, last: 4, .. })),
        "{events:?}"
    );
    assert_eq!(s.entitlements().await, [2 * quarter, 6 * quarter]);

    // 4. Claim on chain against the posted root, relayed by another account; a second claim mints nothing.
    let claimed = s.claim(ALICE).await;
    assert_eq!(claimed, U256::from(2 * quarter));
    assert_eq!(s.refused_claim(ALICE).await, NothingToClaim::SELECTOR);

    // 5. A loss in the open period, a restart after its end, and the first command finalises it.
    s.losses(&[(ALICE, "1")], "100000", "99900").await;
    let open = s.emissions().await.period;
    s.stop().await;
    s.advance_to(schedule.period_end(open) + 1).await;
    s.start_node().await;
    let events = s.submit(Command::Tick).await;
    assert!(
        events.iter().any(|e| matches!(e, Event::EmissionAllocated { account: ALICE, period, .. } if *period == open))
    );
    assert_eq!(s.entitlements().await, [2 * quarter + DEV_PERIOD_BUDGET, 6 * quarter]);
    s.checkpoint_posted().await;
    let replayed = replay_path(&s.dir).expect("the log replays with every checkpoint signature");
    assert_eq!(Some(&s.emissions().await), replayed.engine.emissions());
    assert_eq!(entitlements(&replayed.engine), s.entitlements().await);

    // 6. After the first halving, the same losses share half the budget.
    s.advance_to(schedule.period_end(PERIODS_PER_HALVING - 1)).await;
    s.losses(&[(ALICE, "1"), (BOB, "3")], "100000", "99900").await;
    let events = s.next_period().await;
    assert!(
        events.iter().any(|e| matches!(e, Event::EmissionPeriodClosed { period, budget, .. }
        if *period == PERIODS_PER_HALVING && budget.0 == DEV_PERIOD_BUDGET / 2)),
        "{events:?}"
    );
    let [alice, bob] = s.entitlements().await;
    assert_eq!((alice, bob), (2 * quarter + DEV_PERIOD_BUDGET + quarter / 2, 6 * quarter + 3 * quarter / 2));
    // Every finalised budget is an entitlement or cancelled.
    let emissions = s.emissions().await;
    assert_eq!(emissions.entitled.0 + emissions.cancelled.0, schedule.budgets(0, emissions.period));

    // 7. The later claim mints only the increment, to the full entitlement; repeating it mints nothing.
    assert_eq!(s.claim(ALICE).await, U256::from(alice - 2 * quarter));
    assert_eq!(s.claim(BOB).await, U256::from(bob));
    assert_eq!(s.balance(ALICE).await, U256::from(alice));
    assert_eq!(s.refused_claim(BOB).await, NothingToClaim::SELECTOR);
    s.stop().await;
}

/// The chain, the deployed vault and token, and the node, which the scenario can stop and restart.
struct Scenario {
    _anvil: Anvil,
    provider: RpcClient,
    funder: Address,
    relayer: Address,
    usdc: Address,
    vault: Address,
    monkey: Address,
    key: Arc<EnclaveKey>,
    clock: Arc<TestClock>,
    dir: PathBuf,
    section: CheckpointSection,
    domain: CheckpointDomain,
    /// The dev operator, under the deployed vault's domain.
    operator: Operator,
    node: Option<Node>,
}

/// A running sequencer, its checkpoint worker, and the API over it.
struct Node {
    app: Router,
    state: ApiState,
    sequencer: JoinHandle<anyhow::Result<()>>,
    poster: JoinHandle<anyhow::Result<()>>,
    status: JoinHandle<()>,
}

impl Scenario {
    /// Starts Anvil, deploys a mock USDC and the vault with its MONKEY token, and starts the node.
    async fn start() -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let anvil = Anvil(
            Process::new("anvil")
                .args(["--port", &port.to_string(), "--silent"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("Anvil installed"),
        );
        let url = format!("http://127.0.0.1:{port}");
        let provider = RpcClient::new_http(url.parse().unwrap());
        crate::test_support::wait_for("Anvil to answer eth_chainId", async {
            while provider.request::<_, U64>("eth_chainId", ()).await.is_err() {
                sleep(Duration::from_millis(30)).await;
            }
        })
        .await;
        let accounts: Vec<Address> = provider.request("eth_accounts", ()).await.unwrap();
        let key = Arc::new(EnclaveKey::generate());
        let usdc = deploy(&provider, accounts[0], "MockUSDC.sol/MockUSDC.json", &[]).await;
        let governance = deploy(
            &provider,
            accounts[0],
            "TimelockController.sol/TimelockController.json",
            &(U256::from(1), vec![accounts[0]], vec![Address::ZERO], Address::ZERO).abi_encode_params(),
        )
        .await;
        let args = vault_args(usdc, Address::from(key.address()), governance, accounts[0]);

        let vault = deploy(&provider, accounts[0], "Vault.sol/Vault.json", &args).await;
        let section: CheckpointSection = toml::from_str(&format!(
            "rpc_url = {url:?}\nrelayer = '{:#x}'\nretry_interval = '40ms'\nrpc_timeout = '2s'",
            accounts[9]
        ))
        .unwrap();
        let mut cfg = dev_config();
        cfg.custody.vault = format!("{vault:#x}");
        let mut s = Scenario {
            _anvil: anvil,
            provider,
            funder: accounts[0],
            relayer: accounts[1],
            usdc,
            vault,
            monkey: Address::ZERO,
            key,
            clock: Arc::new(TestClock::at(NOW)),
            dir: scratch_dir("emissions-anvil"),
            section,
            domain: CheckpointDomain { chain_id: 31337, vault: AccountId(vault.into()) },
            operator: cfg.operator().unwrap(),
            node: None,
        };
        s.monkey = s.view(vault, monkeyCall {}).await;
        s.start_node().await;
        s.fund(None, dev_config().genesis.house_capital).await;
        s
    }

    /// Recovers the sequencer from the log, as a node boot does: from the vault's posted checkpoint,
    /// with zero-capital genesis, the V23 rules its unsigned accounts need, and unposted checkpoints queued.
    async fn start_node(&mut self) {
        let poster = Poster {
            chain: CheckpointRpc::new(
                &self.section,
                self.domain,
                self.usdc,
                FinalityTag::Latest,
                0,
                &self.dir,
                AccountId(self.key.address()),
            )
            .unwrap(),
            registry: None,
            domain: self.domain,
            signer: AccountId(self.key.address()),
            retry: self.section.retry_interval,
            pending_deposits_critical_after: Duration::from_secs(1),
            post_withheld: Default::default(),
        };
        let emission_budget = poster.chain.emission_budget().await.unwrap();
        assert_eq!(emission_budget, TokenUnits(DEV_PERIOD_BUDGET), "the vault's MONKEY cap gives its budget back");
        let parent = match poster.chain.state(View::Finalized).await.unwrap().checkpoint {
            Some((seq, root)) => CheckpointParent { has_checkpoint: true, seq, root: root.parse().unwrap() },
            None => CheckpointParent::default(),
        };
        let (work_tx, work_rx) = mpsc::channel(64);
        let (stream, _) = broadcast::channel(1024);
        let (withdrawals, _) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let mut genesis = dev_config().genesis;
        genesis.house_capital = engine_core::Fixed::ZERO;
        let cfg = SequencerConfig {
            segment_commands: 1_000,
            checkpoint_every: CHECKPOINT_EVERY,
            checkpoint_parent: parent,
            checkpoints: Some(work_tx),
            vault: self.domain.vault,
            operator: self.operator,
            clock: self.clock.clone(),
            genesis: crate::sequencer::genesis_commands(&genesis),
        close_spread_ceilings: genesis.close_spread_ceilings.clone(),
            ..sequencer_config(&self.dir, manifest_tx)
        };
        let mut seq = Sequencer::recover(cfg, self.key.clone(), stream.clone(), withdrawals).unwrap();
        seq.activate_legacy_protocol().unwrap();
        if seq.needs_genesis() {
            seq.genesis(&genesis).unwrap();
        }
        seq.trust(sample_around(self.clock.as_ref())).unwrap();
        let recovery = seq.take_recovery_checkpoints();
        let (tx, rx) = mpsc::channel(1024);
        let sequencer = tokio::spawn(seq.run(rx));
        let (status_tx, status, ready) = api::status_channel(tx.clone());
        let poster = tokio::spawn(poster.run_with_recovery(work_rx, status_tx, recovery));
        let unlimited = |_: RateLimit| Quota::per_second(NonZeroU32::MAX);
        let bundle = AttestationBundle {
            enclave_address: self.key.address(),
            config_sha256: [0; 32],
            tls_public_key_sha256: [0; 32],
        };
        let state = ApiState {
            protocol: engine_core::protocol::ProtocolVersion::V23,
            access: None,
            tx,
            stream,
            attestation: Arc::new(attest_bundle(&MockAttestor, &bundle).unwrap()),
            auth: ClientAuth::Unsigned,
            operator: self.operator,
            mock_custody: false,
            payouts_enabled: true,
            clock: || NOW,
            log_dir: self.dir.clone(),
            manifest,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: Arc::new(ApiLimits::with_quota(&dev_config().api, unlimited, DefaultClock::default())),
            history: Reader::new(self.dir.join(INDEX_FILE), watch::channel(true).1),
            emission_budget: Some(emission_budget),
            collateral: None,
            listing: Default::default(),
            signing: crate::test_support::signing(),
        };
        let app = router(state.clone()).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        self.node = Some(Node { app, state, sequencer, poster, status });
        let health = crate::test_support::wait_for("the worker to revalidate custody", ready).await.unwrap();
        assert_eq!(health, Health::Healthy, "the scenario starts from proven healthy collateral");
    }

    /// Stops the checkpoint worker and the sequencer, which drains and writes a final checkpoint.
    async fn stop(&mut self) {
        let Node { app, state, sequencer, poster, status } = self.node.take().expect("the node is running");
        poster.abort();
        let _ = poster.await;
        status.abort();
        let _ = status.await;
        drop((app, state));
        sequencer.await.unwrap().unwrap();
    }

    /// The running node.
    fn node(&self) -> &Node {
        self.node.as_ref().expect("the node is running")
    }

    /// Moves the clock to `ts`, and trusted time with it.
    async fn advance_to(&self, ts: Millis) {
        let by = u64::try_from(ts - self.clock.now_ms()).expect("time only moves forward");
        self.clock.advance(Duration::from_millis(by));
        if let Some(node) = &self.node {
            node.state.tx.send(Request::TimeSample(sample_around(self.clock.as_ref()))).await.unwrap();
        }
    }

    /// Ticks at the end of the open period, finalising it, and returns the tick's events.
    async fn next_period(&self) -> Vec<Event> {
        self.advance_periods(1).await
    }

    /// Ticks `periods` periods after the start of the open one, and returns the tick's events.
    async fn advance_periods(&self, periods: u64) -> Vec<Event> {
        let emissions = self.emissions().await;
        self.advance_to(emissions.schedule.period_end(emissions.period + periods - 1)).await;
        self.submit(Command::Tick).await
    }

    /// Each account buys its quantity of BTC at `from` and sells it at `to` through the API.
    async fn losses(&self, trades: &[(AccountId, &str)], from: &str, to: &str) {
        for (mid, side) in [(from, Side::Buy), (to, Side::Sell)] {
            let ts = self.clock.now_ms();
            let quotes = ["a", "b"].map(|source| SourceQuote {
                source: source.into(),
                mid: mid.parse().unwrap(),
                ts,
                contract: None,
            });
            self.submit(Command::Prices { market: 1, quotes: quotes.to_vec() }).await;
            for (account, qty) in trades {
                let order = OrderRequest {
                    account: *account,
                    client_id: 1,
                    market: 1,
                    side,
                    qty: qty.parse().unwrap(),
                    kind: OrderKind::Market { quoted: mid.parse().unwrap(), max_slippage_bps: None },
                    reduce_only: false,
                    auth: None,
                };
                let (status, events) = self.post("/v1/orders", serde_json::to_value(order).unwrap()).await;
                assert!(status == StatusCode::OK && events.to_string().contains("\"fill\""), "{events}");
            }
        }
    }

    /// Ticks until a checkpoint follows the last command, then waits for the vault to post it.
    async fn checkpoint_posted(&self) {
        while (self.query(|e| e.last_seq().unwrap()).await + 1) % CHECKPOINT_EVERY != 0 {
            self.submit(Command::Tick).await;
        }
        let last = self.query(|e| e.last_seq().unwrap()).await;
        crate::test_support::wait_for("the checkpoint worker to post the checkpoint on chain", async {
            while self.view(self.vault, latestSeqCall {}).await != last {
                sleep(Duration::from_millis(40)).await;
            }
        })
        .await;
    }

    /// Fetches `account`'s entitlement proof from the API once the vault has posted its checkpoint,
    /// and has the relayer claim it; returns the MONKEY minted.
    async fn claim(&self, account: AccountId) -> U256 {
        let proof = self.posted_proof(account).await;
        let before = self.balance(account).await;
        send(&self.provider, self.relayer, self.vault, claim_call(&proof)).await;
        assert_eq!(self.balance(AccountId(self.relayer.0.0)).await, U256::ZERO, "the relayer receives nothing");
        self.balance(account).await - before
    }

    /// The selector the vault reverts with when the relayer claims `account`'s current proof.
    async fn refused_claim(&self, account: AccountId) -> [u8; 4] {
        let proof = self.posted_proof(account).await;
        let call = TransactionRequest::default().from(self.relayer).to(self.vault).input(claim_call(&proof).into());
        let error = self.provider.request::<_, Bytes>("eth_call", (call, "latest")).await.unwrap_err();
        let data = error.as_error_resp().and_then(|payload| payload.as_revert_data()).expect("a revert");
        data[..4].try_into().unwrap()
    }

    /// `account`'s proof from the API against the root the vault holds.
    async fn posted_proof(&self, account: AccountId) -> EntitlementProof {
        self.checkpoint_posted().await;
        let (status, body) = self.get(&format!("/v1/accounts/{account}/emissions")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let proof: EntitlementProof = serde_json::from_value(body).unwrap();
        assert_eq!(proof.root, self.view(self.vault, latestRootCall {}).await, "the proof is for the posted root");
        proof
    }

    /// `account`'s MONKEY balance.
    async fn balance(&self, account: AccountId) -> U256 {
        self.view(self.monkey, balanceOfCall { account: account.0.into() }).await
    }

    /// Alice's and Bob's entitlements in base units.
    async fn entitlements(&self) -> [u128; 2] {
        self.query(entitlements).await
    }

    /// The engine's emission state.
    async fn emissions(&self) -> Emissions {
        self.query(|e| e.emissions().expect("initialised").clone()).await
    }

    /// `command` signed by the dev operator key with the operator's next nonce, as `engine admin`
    /// posts it.
    fn signed(&self, command: AdminCommand) -> Value {
        let key = k256::ecdsa::SigningKey::from_slice(&[0x42; 32]).unwrap();
        serde_json::to_value(crate::admin::sign(&key, &self.operator, command, 0, NOW + 60_000)).unwrap()
    }

    /// Sequences `cmd` and returns its events.
    async fn submit(&self, cmd: Command) -> Vec<Event> {
        let (reply, rx) = oneshot::channel();
        self.node().state.tx.send(Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
        crate::test_support::wait_for("the sequencer to answer a submission", rx)
            .await
            .expect("trusted time admits the command")
    }

    /// Reads the engine between batches.
    async fn query<T: Send + 'static>(&self, read: impl FnOnce(&Engine) -> T + Send + 'static) -> T {
        let (reply, rx) = oneshot::channel();
        let query = Box::new(move |e: &Engine| {
            let _ = reply.send(read(e));
        });
        self.node().state.tx.send(Request::Query(query)).await.unwrap();
        rx.await.unwrap()
    }
}

/// Alice's and Bob's entitlements in base units.
fn entitlements(engine: &Engine) -> [u128; 2] {
    [ALICE, BOB].map(|account| engine.entitlement(account).0)
}

/// The vault call that claims `proof`.
fn claim_call(proof: &EntitlementProof) -> Vec<u8> {
    let siblings: Vec<B256> = proof.siblings.clone();
    claimEmissionsCall { account: proof.account.0.into(), entitled: U256::from(proof.entitled.0), siblings }
        .abi_encode()
}

/// Encodes the Vault's constructor arguments: token, signer, governance, guardian, budget, and the
/// thirty-day exit schedule.
fn vault_args(usdc: Address, signer: Address, governance: Address, guardian: Address) -> Vec<u8> {
    let schedule = crate::test_support::DEV_EXIT_SCHEDULE;
    (usdc, signer, governance, guardian, U256::from(DEV_PERIOD_BUDGET), schedule.0, schedule.1, schedule.2).abi_encode()
}
