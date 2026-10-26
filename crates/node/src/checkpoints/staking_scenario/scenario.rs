//! The chain, the deployed vault, token, and staking contract, and the node the staking scenario
//! drives, which it can stop and restart.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::{Command as Process, Stdio};
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, Bytes, U64, U256};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{SolCall, SolValue};
use axum::Router;
use axum::extract::connect_info::MockConnectInfo;
use engine_core::Fixed;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::emissions::TokenUnits;
use engine_core::staking::StakingConfig;
use engine_core::types::AccountId;
use governor::Quota;
use governor::clock::DefaultClock;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use super::{
    CHECKPOINT_EVERY, COOLDOWN_MS, COOLDOWN_SECS, DEV_PERIOD_BUDGET, EPOCH_MS, FLOOR, NOW, mintCall, monkeyCall,
};
use crate::api::{ApiState, MAX_BACKFILLS, router};
use crate::checkpoints::scenario::{Anvil, deploy};
use crate::checkpoints::{CheckpointChain, CheckpointRpc, Poster, View};
use crate::clock::test_clock::TestClock;
use crate::config::{CheckpointSection, FinalityTag, Genesis, RateLimit};
use crate::history::{INDEX_FILE, Reader};
use crate::limits::ApiLimits;
use crate::log::{CheckpointDomain, CheckpointParent, Manifest};
use crate::sequencer::{Sequencer, SequencerConfig, genesis_commands};
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{dev_config, sample_around, scratch_dir, sequencer_config};

/// The chain, the deployed vault, token, and staking contract, and the node, which the scenario
/// can stop and restart.
pub(super) struct Scenario {
    pub(super) _anvil: Anvil,
    pub(super) url: String,
    pub(super) funder: Address,
    pub(super) provider: RpcClient,
    pub(super) relayer: Address,
    pub(super) usdc: Address,
    pub(super) vault: Address,
    pub(super) monkey: Address,
    pub(super) staking: Address,
    pub(super) stakers: [AccountId; 2],
    pub(super) key: Arc<EnclaveKey>,
    pub(super) clock: Arc<TestClock>,
    pub(super) dir: PathBuf,
    section: CheckpointSection,
    domain: CheckpointDomain,
    pub(super) genesis: Genesis,
    pub(super) operator: Operator,
    /// Dev deposits credited so far, part of the liabilities the vault must hold.
    pub(super) deposits: Fixed,
    pub(super) node: Option<Node>,
}

/// A running sequencer, its checkpoint worker, and the API over it.
pub(super) struct Node {
    pub(super) app: Router,
    pub(super) state: ApiState,
    sequencer: JoinHandle<anyhow::Result<()>>,
    poster: JoinHandle<anyhow::Result<()>>,
}

impl Scenario {
    /// Starts Anvil, deploys a mock USDC, the vault with its MONKEY token, and the staking
    /// contract, and starts the node with the staking config in its genesis.
    pub(super) async fn start() -> Self {
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
        timeout(Duration::from_secs(10), async {
            while provider.request::<_, U64>("eth_chainId", ()).await.is_err() {
                sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        let accounts: Vec<Address> = provider.request("eth_accounts", ()).await.unwrap();
        let key = Arc::new(EnclaveKey::generate());
        let usdc = deploy(&provider, accounts[0], "MockUSDC.sol/MockUSDC.json", &[]).await;
        let args = (usdc, Address::from(key.address()), U256::from(DEV_PERIOD_BUDGET)).abi_encode();
        let vault = deploy(&provider, accounts[0], "Vault.sol/Vault.json", &args).await;
        let monkey = {
            let request = TransactionRequest::default().to(vault).input(monkeyCall {}.abi_encode().into());
            let out: Bytes = provider.request("eth_call", (request, "latest")).await.unwrap();
            monkeyCall::abi_decode_returns(&out).unwrap()
        };
        let staking = deploy(
            &provider,
            accounts[0],
            "Staking.sol/Staking.json",
            &(monkey, U256::from(COOLDOWN_SECS)).abi_encode(),
        )
        .await;
        let mut genesis = dev_config().genesis;
        genesis.staking =
            Some(StakingConfig { liquidity_floor: FLOOR, epoch_ms: EPOCH_MS, start_ms: NOW, cooldown_ms: COOLDOWN_MS });
        let section: CheckpointSection = toml::from_str(&format!(
            "rpc_url = {url:?}\nrelayer = '{:#x}'\nretry_interval = '40ms'\nrpc_timeout = '2s'",
            accounts[9]
        ))
        .unwrap();
        let mut s = Scenario {
            _anvil: anvil,
            url,
            funder: accounts[0],
            provider,
            relayer: accounts[1],
            usdc,
            vault,
            monkey,
            staking,
            stakers: [AccountId(accounts[2].into()), AccountId(accounts[3].into())],
            key,
            clock: Arc::new(TestClock::at(NOW)),
            dir: scratch_dir("staking-anvil"),
            section,
            domain: CheckpointDomain { chain_id: 31337, vault: AccountId(vault.into()) },
            genesis,
            operator: {
                // The operator signs under the deployed vault's domain, as the log records it.
                let mut cfg = dev_config();
                cfg.custody.vault = format!("{vault:#x}");
                cfg.operator().unwrap()
            },
            deposits: Fixed::ZERO,
            node: None,
        };
        // The vault backs the genesis house capital and the dev deposits the scenario credits.
        let liabilities = s.liabilities();
        s.send_tx(accounts[0], s.usdc, mintCall { to: s.vault, amount: liabilities }.abi_encode()).await;
        s.start_node().await;
        s
    }

    /// Recovers the sequencer from the log, as a node boot does: from the vault's posted checkpoint,
    /// with the genesis that stakes on a fresh log, and the unposted checkpoints queued.
    pub(super) async fn start_node(&mut self) {
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
        };
        let parent = match poster.chain.state(View::Finalized).await.unwrap().checkpoint {
            Some((seq, root)) => CheckpointParent { has_checkpoint: true, seq, root: root.parse().unwrap() },
            None => CheckpointParent::default(),
        };
        let (work_tx, work_rx) = mpsc::channel(64);
        let (stream, _) = broadcast::channel(1024);
        let (withdrawals, _) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let cfg = SequencerConfig {
            segment_commands: 1_000,
            checkpoint_every: CHECKPOINT_EVERY,
            checkpoint_parent: parent,
            checkpoints: Some(work_tx),
            vault: self.domain.vault,
            operator: self.operator,
            clock: self.clock.clone(),
            genesis: genesis_commands(&self.genesis),
            ..sequencer_config(&self.dir, manifest_tx)
        };
        let mut seq = Sequencer::recover(cfg, self.key.clone(), stream.clone(), withdrawals).unwrap();
        seq.activate_protocol().unwrap();
        if seq.needs_genesis() {
            seq.genesis(&self.genesis).unwrap();
        }
        seq.trust(sample_around(self.clock.as_ref())).unwrap();
        let recovery = seq.take_recovery_checkpoints();
        let (tx, rx) = mpsc::channel(1024);
        let sequencer = tokio::spawn(seq.run(rx));
        let poster = tokio::spawn(poster.run_with_recovery(work_rx, tx.clone(), recovery));
        let unlimited = |_: RateLimit| Quota::per_second(NonZeroU32::MAX);
        let bundle = AttestationBundle {
            enclave_address: self.key.address(),
            config_sha256: [0; 32],
            tls_public_key_sha256: [0; 32],
        };
        let state = ApiState {
            tx,
            stream,
            attestation: Arc::new(attest_bundle(&MockAttestor, &bundle).unwrap()),
            auth: ClientAuth::Unsigned,
            operator: self.operator,
            mock_custody: true,
            clock: || NOW,
            log_dir: self.dir.clone(),
            manifest,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: Arc::new(ApiLimits::with_quota(&dev_config().api, unlimited, DefaultClock::default())),
            history: Reader::new(self.dir.join(INDEX_FILE), watch::channel(true).1),
            emission_budget: Some(TokenUnits(DEV_PERIOD_BUDGET)),
            payouts_enabled: true,
            listing: Default::default(),
        };
        let app = router(state.clone()).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        self.node = Some(Node { app, state, sequencer, poster });
    }

    /// Stops the checkpoint worker and the sequencer, which drains and writes a final checkpoint.
    pub(super) async fn stop(&mut self) {
        let Node { app, state, sequencer, poster } = self.node.take().expect("the node is running");
        poster.abort();
        let _ = poster.await;
        drop((app, state));
        sequencer.await.unwrap().unwrap();
    }

    pub(super) fn node(&self) -> &Node {
        self.node.as_ref().expect("the node is running")
    }
}
