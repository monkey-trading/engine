//! The election scenario's chain: Anvil at a fixed genesis time, three local RPC proxies for the
//! custody quorum, and the vault, MONKEY, operator registry, and the staking contract it deploys.

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

sol! {
    struct InitialOperator { address adminKey; bytes launchDigest; bytes32 configHash; }
    struct Parameters {
        uint64 epochLength;
        uint64 minTerm;
        uint64 finalityMargin;
        uint16 marginBps;
        uint16 eligibilityBps;
        uint16 quorumBps;
        uint16 maxCandidates;
        uint256 minSelfBond;
    }
    struct Registration {
        address adminKey;
        uint256 bond;
        bytes launchDigest;
        bytes32 configHash;
        string apiHostname;
        string handoverEndpoint;
    }
    struct Operator { address candidate; address adminKey; bytes32 release; uint64 since; }
    function monkey() external view returns (address);
    function staking() external view returns (address);
    function mint(address to, uint256 amount) external;
    function approve(address spender, uint256 amount) external returns (bool);
    function stake(uint256 amount) external;
    function startUnstake(uint256 amount) external;
    function delegate(address candidate) external;
    function register(Registration registration) external;
    function setReleaseApproval(address candidate, bytes launchDigest, bytes32 configHash, bool approved) external;
    function setRotation(bool enabled) external;
    function closeEpoch() external returns (uint64);
    function operatorInEffect() external view returns (Operator);
    function schedule(address target, uint256 value, bytes data, bytes32 predecessor, bytes32 salt, uint256 delay) external;
    function execute(address target, uint256 value, bytes payload, bytes32 predecessor, bytes32 salt) external payable;
    error SelfBondLocked(uint256 locked);
}

/// The local chain and the contracts the scenario deployed.
pub(super) struct Chain {
    _anvil: Anvil,
    _proxies: Vec<Anvil>,
    pub(super) provider: RpcClient,
    url: String,
    /// The URLs of the three proxies the custody quorum reads through.
    pub(super) proxies: Vec<String>,
    pub(super) accounts: Vec<Address>,
    pub(super) usdc: Address,
    pub(super) vault: Address,
    pub(super) monkey: Address,
    pub(super) governance: Address,
    pub(super) registry: Address,
    pub(super) staking: Address,
    salts: u64,
}

impl Chain {
    /// Starts Anvil with its genesis at `T0_SECS` and the quorum's proxies, and deploys a mock USDC,
    /// the timelock, the vault with its MONKEY, and the registry, with `deployer_key` the operator
    /// in effect.
    pub(super) async fn start(enclave: Address, deployer_key: Address) -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let anvil = Anvil(
            Process::new("anvil")
                .args(["--port", &port.to_string(), "--silent", "--timestamp", &T0_SECS.to_string()])
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
            salts: 0,
        };
        chain.monkey = chain.view(vault, monkeyCall {}).await;
        let initial = InitialOperator {
            adminKey: deployer_key,
            launchDigest: Bytes::from(DEPLOYER_DIGEST.to_vec()),
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

    /// Registers `candidate` with `admin_key`, a self-bond of `bond`, and the release `digest`.
    pub(super) async fn register(&self, candidate: Address, admin_key: Address, bond: U256, digest: [u8; 48]) {
        let registration = Registration {
            adminKey: admin_key,
            bond,
            launchDigest: Bytes::from(digest.to_vec()),
            configHash: CONFIG_HASH.into(),
            apiHostname: "api.candidate.local".into(),
            handoverEndpoint: "127.0.0.1:9443".into(),
        };
        self.send(candidate, self.registry, registerCall { registration }.abi_encode()).await;
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
        let from = self.accounts[0];
        let mut salts = Vec::new();
        for data in &calls {
            self.salts += 1;
            let salt = B256::from(U256::from(self.salts));
            let schedule = scheduleCall {
                target: self.registry,
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
        for (data, salt) in calls.into_iter().zip(salts) {
            let execute = executeCall {
                target: self.registry,
                value: U256::ZERO,
                payload: data.into(),
                predecessor: B256::ZERO,
                salt,
            };
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

    /// The revert data of a call from `from` that must revert.
    pub(super) async fn reverted(&self, from: Address, to: Address, input: Vec<u8>) -> Bytes {
        let request = TransactionRequest::default().from(from).to(to).input(input.into());
        let error = self.provider.request::<_, Bytes>("eth_call", (request, "latest")).await.unwrap_err();
        error.as_error_resp().and_then(|e| e.as_revert_data()).expect("the call reverts with data")
    }

    /// A view call at the latest block.
    pub(super) async fn view<C: SolCall>(&self, to: Address, call: C) -> C::Return {
        let request = TransactionRequest::default().to(to).input(call.abi_encode().into());
        let out: Bytes = self.provider.request("eth_call", (request, "latest")).await.unwrap();
        C::abi_decode_returns(&out).unwrap()
    }

    /// Sends a transaction from an unlocked or impersonated account, asserting it succeeds.
    pub(super) async fn send(&self, from: Address, to: Address, input: Vec<u8>) -> TransactionReceipt {
        let receipt =
            transact(&self.provider, TransactionRequest::default().from(from).to(to).input(input.into())).await;
        assert!(receipt.status(), "transaction to {to} reverted");
        receipt
    }

    async fn next_block_at(&self, at: u64) {
        self.rpc("evm_setNextBlockTimestamp", json!([at])).await;
    }

    async fn latest_timestamp(&self) -> u64 {
        let block: Value = self.provider.request("eth_getBlockByNumber", ("latest", false)).await.unwrap();
        u64::from_str_radix(block["timestamp"].as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
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
