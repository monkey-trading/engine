//! The election scenarios' chain: Anvil at a fixed genesis time, three local RPC proxies for the
//! custody quorum, and the vault, MONKEY, operator registry, and the staking contract it deploys.
//! A chain may finalise each block only two blocks after it, for a scenario whose readers wait on
//! finality.

use std::process::{Command as Process, Stdio};
use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, U64, U256};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use serde_json::{Value, json};
use tokio::time::sleep;

use super::{
    CONFIG_HASH, COOLDOWN_SECS, DEPLOYER_DIGEST, DISTRIBUTION_EPOCH_SECS, GOVERNANCE_DELAY_SECS, T0_SECS,
    dev_parameters,
};
use crate::checkpoints::scenario::{Anvil, deploy, transact};
use crate::config::FinalityTag;

/// The handover endpoint a candidate registers when its scenario runs no listener.
const UNSERVED_ENDPOINT: &str = "127.0.0.1:9443";

sol! {
    struct InitialOperator { address adminKey; bytes launchDigest; bytes32 configHash; }
    struct Parameters {
        uint64 epochLength;
        uint64 finalityMargin;
        uint16 eligibilityBps;
        uint16 quorumBps;
        uint16 maxCandidates;
        uint256 minSelfBond;
        uint64 livenessTimeout;
    }
    struct Registration {
        address adminKey;
        address enclave;
        uint256 bond;
        bytes launchDigest;
        bytes32 configHash;
        string apiHostname;
        string handoverEndpoint;
    }
    struct Operator { address candidate; address adminKey; bytes32 release; uint64 since; }
    struct Delegation { address candidate; uint256 amount; }
    function monkey() external view returns (address);
    function staking() external view returns (address);
    function mint(address to, uint256 amount) external;
    function approve(address spender, uint256 amount) external returns (bool);
    function stake(uint256 amount) external;
    function startUnstake(uint256 amount) external;
    function delegate(address candidate, uint256 amount) external;
    function undelegate(address candidate, uint256 amount) external;
    function redelegate(address from, address to, uint256 amount) external;
    function delegationsOf(address account) external view returns (Delegation[]);
    function registrationDigest(address candidate, address adminKey) external view returns (bytes32);
    function register(Registration registration, bytes signature) external;
    function setReleaseApproval(address candidate, bytes launchDigest, bytes32 configHash, bool approved) external;
    function setRotation(bool enabled) external;
    function closeEpoch() external returns (uint64);
    function setParameters(Parameters parameters) external;
    function votes(address candidate) external view returns (uint256);
    function operatorInEffect() external view returns (Operator);
    function depositToHouse(uint256 amount) external returns (uint256);
    function depositFor(address account, uint256 amount) external returns (uint256);
    function schedule(address target, uint256 value, bytes data, bytes32 predecessor, bytes32 salt, uint256 delay) external;
    function execute(address target, uint256 value, bytes payload, bytes32 predecessor, bytes32 salt) external payable;
    error SelfBondLocked(uint256 locked);
}

/// The local chain and the contracts the scenario deployed.
pub(super) struct Chain {
    _anvil: Anvil,
    _proxies: Vec<Anvil>,
    pub(super) provider: RpcClient,
    pub(super) url: String,
    /// The URLs of the three proxies the custody quorum reads through.
    pub(super) proxies: Vec<String>,
    pub(super) accounts: Vec<Address>,
    pub(super) usdc: Address,
    pub(super) vault: Address,
    pub(super) monkey: Address,
    pub(super) governance: Address,
    pub(super) registry: Address,
    pub(super) staking: Address,
    /// The `RegistryLens` beside the registry, once a scenario deploys one.
    pub(super) lens: Option<Address>,
    /// The `OperatorSchedule` the registry names, once a scenario deploys one.
    pub(super) schedule: Option<Address>,
    /// The `ParameterVote` over the schedule and the restriction lifetime the node records with it, once
    /// a scenario deploys one.
    pub(super) vote: Option<(Address, engine_core::types::Millis)>,
    /// The tag the custody watcher's staking mirror and the vault's readers take as final: the head,
    /// or on a chain started with `start_with_trailing_finality`, Anvil's finalized block.
    pub(super) release_tag: FinalityTag,
    salts: u64,
}

impl Chain {
    /// Starts Anvil with its genesis at `T0_SECS` and the quorum's proxies, and deploys a mock USDC,
    /// the timelock, the vault with its MONKEY, and the registry, with `deployer_key` the operator
    /// in effect.
    pub(super) async fn start(enclave: Address, deployer_key: Address) -> Self {
        Self::launch(enclave, deployer_key, DEPLOYER_DIGEST, FinalityTag::Latest).await
    }

    /// `start`, with `operator_key` the operator in effect running the release `digest`, and Anvil
    /// finalising each block once two more follow it (`finalize`), which the staking mirror and the
    /// vault's readers wait for.
    pub(super) async fn start_with_trailing_finality(
        enclave: Address,
        operator_key: Address,
        digest: [u8; 48],
    ) -> Self {
        Self::launch(enclave, operator_key, digest, FinalityTag::Finalized).await
    }

    async fn launch(enclave: Address, deployer_key: Address, digest: [u8; 48], release_tag: FinalityTag) -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        // One slot per epoch: Anvil's finalized block is two below its head.
        let trailing: &[&str] = if release_tag == FinalityTag::Finalized { &["--slots-in-an-epoch", "1"] } else { &[] };
        let anvil = Anvil(
            Process::new("anvil")
                .args(["--port", &port.to_string(), "--silent", "--timestamp", &T0_SECS.to_string()])
                .args(trailing)
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
        let (proxies, urls) = proxies(&url).await;
        let accounts: Vec<Address> = provider.request("eth_accounts", ()).await.unwrap();
        let from = accounts[0];
        let usdc = deploy(&provider, from, "MockUSDC.sol/MockUSDC.json", &[]).await;
        let delay = U256::from(GOVERNANCE_DELAY_SECS);
        let timelock = (delay, vec![from], vec![from], Address::ZERO).abi_encode_params();
        let governance = deploy(&provider, from, "TimelockController.sol/TimelockController.json", &timelock).await;
        // The MONKEY period budget, 10 MONKEY, is a development value, not an approved production
        // parameter; it caps MONKEY at 525,600, above the scenario's stakes.
        let budget = U256::from(10).pow(U256::from(19));
        let schedule = crate::test_support::DEV_EXIT_SCHEDULE;
        let vault = deploy(
            &provider,
            from,
            "Vault.sol/Vault.json",
            &(usdc, enclave, governance, from, budget, schedule.0, schedule.1, schedule.2).abi_encode(),
        )
        .await;
        let mut chain = Chain {
            _anvil: anvil,
            _proxies: proxies,
            provider,
            url,
            proxies: urls,
            accounts,
            usdc,
            vault,
            monkey: Address::ZERO,
            governance,
            registry: Address::ZERO,
            staking: Address::ZERO,
            lens: None,
            schedule: None,
            vote: None,
            release_tag,
            salts: 0,
        };
        chain.monkey = chain.view(vault, monkeyCall {}).await;
        let initial = InitialOperator {
            adminKey: deployer_key,
            launchDigest: Bytes::from(digest.to_vec()),
            configHash: CONFIG_HASH.into(),
        };
        let (parameters, minimums, maximums) = dev_parameters();
        let args = (
            chain.monkey,
            U256::from(COOLDOWN_SECS),
            U256::from(DISTRIBUTION_EPOCH_SECS),
            governance,
            initial,
            parameters,
            minimums,
            maximums,
        )
            .abi_encode_params();
        chain.registry = deploy(&chain.provider, from, "OperatorRegistry.sol/OperatorRegistry.json", &args).await;
        chain.staking = chain.view(chain.registry, stakingCall {}).await;
        chain
    }

    /// Deploys the registry lens beside the registry.
    pub(super) async fn deploy_lens(&mut self) {
        let args = self.registry.abi_encode();
        self.lens = Some(deploy(&self.provider, self.accounts[0], "RegistryLens.sol/RegistryLens.json", &args).await);
    }

    /// A fresh account no key is held for: Anvil signs for it, and it holds gas.
    pub(super) async fn impersonated(&self, seed: &str) -> Address {
        let account = Address::from_slice(&alloy::primitives::keccak256(seed.as_bytes())[12..]);
        self.impersonate(account).await;
        account
    }

    /// Has Anvil sign for `account`, which it funds with gas.
    pub(super) async fn impersonate(&self, account: Address) {
        self.rpc("anvil_setBalance", json!([account, "0xde0b6b3a7640000"])).await;
        self.rpc("anvil_impersonateAccount", json!([account])).await;
    }

    /// Mints `amount` MONKEY to `account` as the vault, its only minter, in place of a proven
    /// emission claim, then stakes it all.
    pub(super) async fn stake(&self, account: Address, amount: U256) -> TransactionReceipt {
        self.rpc("anvil_impersonateAccount", json!([self.vault])).await;
        self.rpc("anvil_setBalance", json!([self.vault, "0xde0b6b3a7640000"])).await;
        self.send(self.vault, self.monkey, mintCall { to: account, amount }.abi_encode()).await;
        self.rpc("anvil_stopImpersonatingAccount", json!([self.vault])).await;
        self.send(account, self.monkey, approveCall { spender: self.staking, amount }.abi_encode()).await;
        self.send(account, self.staking, stakeCall { amount }.abi_encode()).await
    }

    /// Deposits `amount` USDC base units into the vault for the house, or for `account`, minting them
    /// to the funder first; the custody watcher credits the deposit from its event.
    pub(super) async fn fund(&self, account: Option<Address>, amount: U256) {
        let funder = self.accounts[0];
        self.send(funder, self.usdc, mintCall { to: funder, amount }.abi_encode()).await;
        self.send(funder, self.usdc, approveCall { spender: self.vault, amount }.abi_encode()).await;
        let deposit = match account {
            Some(account) => depositForCall { account, amount }.abi_encode(),
            None => depositToHouseCall { amount }.abi_encode(),
        };
        self.send(funder, self.vault, deposit).await;
    }

    /// Registers `candidate` with `admin_key` — which must authorize the registration with its
    /// EIP-712 signature, as `register` checks — a self-bond of `bond`, and the release `digest`.
    pub(super) async fn register(
        &self,
        candidate: Address,
        admin_key: &k256::ecdsa::SigningKey,
        bond: U256,
        digest: [u8; 48],
    ) {
        self.register_serving(candidate, admin_key, bond, digest, UNSERVED_ENDPOINT).await;
    }

    /// `register`, with the handover listener at `endpoint` that a successor takes the keys from.
    pub(super) async fn register_serving(
        &self,
        candidate: Address,
        admin_key: &k256::ecdsa::SigningKey,
        bond: U256,
        digest: [u8; 48],
        endpoint: &str,
    ) {
        self.register_serving_at(candidate, admin_key, bond, digest, endpoint, "api.candidate.local").await;
    }

    /// `register_serving`, with the API hostname `api_hostname`, where the candidate's node serves.
    pub(super) async fn register_serving_at(
        &self,
        candidate: Address,
        admin_key: &k256::ecdsa::SigningKey,
        bond: U256,
        digest: [u8; 48],
        endpoint: &str,
        api_hostname: &str,
    ) {
        let admin = Address::from(engine_core::eip712::address(admin_key.verifying_key()).0);
        let registration = Registration {
            adminKey: admin,
            enclave: admin,
            bond,
            launchDigest: Bytes::from(digest.to_vec()),
            configHash: CONFIG_HASH.into(),
            apiHostname: api_hostname.into(),
            handoverEndpoint: endpoint.into(),
        };
        let digest: B256 = self.view(self.registry, registrationDigestCall { candidate, adminKey: admin }).await;
        let signature = engine_core::eip712::sign(admin_key, &digest);
        self.send(
            candidate,
            self.registry,
            registerCall { registration, signature: signature.to_vec().into() }.abi_encode(),
        )
        .await;
    }

    /// Has the timelock approve `candidate` running the release `digest`.
    pub(super) fn approval(&self, candidate: Address, digest: [u8; 48]) -> Vec<u8> {
        let digest = Bytes::from(digest.to_vec());
        setReleaseApprovalCall { candidate, launchDigest: digest, configHash: CONFIG_HASH.into(), approved: true }
            .abi_encode()
    }

    /// Has the timelock make each call to the registry: schedules them, moves the next block past the
    /// delay, and executes them.
    pub(super) async fn govern(&mut self, calls: Vec<Vec<u8>>) {
        let registry = self.registry;
        self.govern_each(calls.into_iter().map(|call| (registry, call)).collect()).await;
    }

    /// `govern`, each call to its own target.
    pub(super) async fn govern_each(&mut self, calls: Vec<(Address, Vec<u8>)>) {
        let from = self.accounts[0];
        let mut salts = Vec::new();
        for (target, data) in &calls {
            self.salts += 1;
            let salt = B256::from(U256::from(self.salts));
            let schedule = scheduleCall {
                target: *target,
                value: U256::ZERO,
                data: data.clone().into(),
                predecessor: B256::ZERO,
                salt,
                delay: U256::from(GOVERNANCE_DELAY_SECS),
            };
            self.send(from, self.governance, schedule.abi_encode()).await;
            salts.push(salt);
        }
        self.next_block_at(self.latest_timestamp().await + GOVERNANCE_DELAY_SECS).await;
        for ((target, data), salt) in calls.into_iter().zip(salts) {
            let execute =
                executeCall { target, value: U256::ZERO, payload: data.into(), predecessor: B256::ZERO, salt };
            self.send(from, self.governance, execute.abi_encode()).await;
        }
    }

    /// Closes the open election epoch in a block stamped `at`, returning its receipt.
    pub(super) async fn close_epoch_at(&self, at: u64) -> TransactionReceipt {
        self.next_block_at(at).await;
        self.send(self.accounts[1], self.registry, closeEpochCall {}.abi_encode()).await
    }

    /// Mines one block stamped `at`, returning its number.
    pub(super) async fn mine_at(&self, at: u64) -> u64 {
        self.next_block_at(at).await;
        self.rpc("anvil_mine", json!(["0x1"])).await;
        self.provider.request::<_, U64>("eth_blockNumber", ()).await.unwrap().to()
    }

    /// Turns Anvil's mining of each transaction as it arrives off, or back on with a block for those
    /// that waited: while it is off the chain mines only the blocks the scenario asks for.
    pub(super) async fn automine(&self, on: bool) {
        self.rpc("evm_setAutomine", json!([on])).await;
        if on {
            self.rpc("anvil_mine", json!(["0x1"])).await;
        }
    }

    /// Mines the two blocks after the head, which Anvil's finalized block then reaches on a chain
    /// started with `start_with_trailing_finality`.
    pub(super) async fn finalize(&self) {
        self.rpc("anvil_mine", json!(["0x2"])).await;
    }

    /// The revert data of a call from `from` that must revert.
    pub(super) async fn reverted(&self, from: Address, to: Address, input: Vec<u8>) -> Bytes {
        let request = TransactionRequest::default().from(from).to(to).input(input.into());
        let error = self.provider.request::<_, Bytes>("eth_call", (request, "latest")).await.unwrap_err();
        error.as_error_resp().and_then(|e| e.as_revert_data()).expect("the call reverts with data")
    }

    /// A view call at the latest block.
    pub(super) async fn view<C: SolCall>(&self, to: Address, call: C) -> C::Return {
        self.view_at(to, call, "latest").await
    }

    /// A view call at the block `tag` names.
    pub(super) async fn view_at<C: SolCall>(&self, to: Address, call: C, tag: &str) -> C::Return {
        let request = TransactionRequest::default().to(to).input(call.abi_encode().into());
        let out: Bytes = self.provider.request("eth_call", (request, tag)).await.unwrap();
        C::abi_decode_returns(&out).unwrap()
    }

    /// The timestamp of the block `tag` names.
    pub(super) async fn timestamp_at(&self, tag: &str) -> u64 {
        let block: Value = self.provider.request("eth_getBlockByNumber", (tag, false)).await.unwrap();
        u64::from_str_radix(block["timestamp"].as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
    }

    /// Sends a transaction from an unlocked or impersonated account, asserting it succeeds.
    pub(super) async fn send(&self, from: Address, to: Address, input: Vec<u8>) -> TransactionReceipt {
        let receipt =
            transact(&self.provider, TransactionRequest::default().from(from).to(to).input(input.into())).await;
        assert!(receipt.status(), "transaction to {to} reverted");
        receipt
    }

    pub(super) async fn next_block_at(&self, at: u64) {
        self.rpc("evm_setNextBlockTimestamp", json!([at])).await;
    }

    pub(super) async fn latest_timestamp(&self) -> u64 {
        self.timestamp_at("latest").await
    }

    /// One Anvil control RPC, asserted to succeed.
    async fn rpc(&self, method: &str, params: Value) {
        let response: Value = reqwest::Client::new()
            .post(&self.url)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response.get("error").is_none(), "{method}: {response}");
    }
}

/// Three local proxies to the same Anvil, for the custody quorum; production uses independent
/// operators.
async fn proxies(target: &str) -> (Vec<Anvil>, Vec<String>) {
    let mut children = Vec::new();
    let mut urls = Vec::new();
    for _ in 0..3 {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        children.push(Anvil(
            Process::new("python3")
                .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/dev-rpc-proxy.py"))
                .args(["--port", &port.to_string(), "--target", target])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("Python3 installed"),
        ));
        crate::test_support::wait_for("the RPC proxy to accept connections", async {
            while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
                sleep(Duration::from_millis(30)).await;
            }
        })
        .await;
        urls.push(format!("http://127.0.0.1:{port}"));
    }
    (children, urls)
}
