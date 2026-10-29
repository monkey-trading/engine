//! Local unlocked Anvil transactions and the same Vault ABI used by real clients.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::TransactionReceipt;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use anyhow::{Context, ensure};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, timeout};

use crate::log::CheckpointDomain;
use crate::sequencer::SignedWithdrawal;
use crate::sequencer::withdrawal_scenario::node::{ChildProcess, free_port};
use crate::tee::EnclaveKey;
use crate::test_support::LOADED_WAIT;
use crate::test_support::dev_config;

sol! {
    struct Withdrawal {
        address account;
        uint256 amount;
        uint64 nonce;
        uint64 approvedSeq;
        uint64 signerEpoch;
        uint64 issuedAt;
        uint64 expiresAt;
    }
    function withdraw(Withdrawal withdrawal, uint64 checkpointSeq, bytes32[] siblings, bytes signature) external;
    function mint(address to, uint256 amount) external;
    function approve(address spender, uint256 amount) external returns (bool);
    function deposit(uint256 amount) external returns (uint256);
    function depositToHouse(uint256 amount) external returns (uint256);
    function balanceOf(address owner) external view returns (uint256);
    function totalWithdrawn() external view returns (uint256);
    function latestSeq() external view returns (uint64);
    function usedNonces(address account, uint64 nonce) external view returns (bool);
    function withdrawalsPaused() external view returns (bool);
    function pauseWithdrawals() external;
    function retireSigner() external;
    function setSigner(address signer) external;
    function setPauses(bool depositsPaused, bool withdrawalsPaused) external;
    function schedule(address target, uint256 value, bytes data, bytes32 predecessor, bytes32 salt, uint256 delay) external;
    function execute(address target, uint256 value, bytes payload, bytes32 predecessor, bytes32 salt) external payable;
    event WithdrawalPaid(address indexed account, uint256 amount, uint64 indexed nonce, uint64 approvedSeq);
}

pub(super) struct Chain {
    pub url: String,
    pub token: Address,
    pub vault: Address,
    pub relayer: Address,
    pub payer: Address,
    pub funder: Address,
    pub signer: engine_core::types::AccountId,
    /// Held for writing while a block is mined; node RPC proxies hold it for reading.
    pub mining: Arc<RwLock<()>>,
    client: reqwest::Client,
    /// Deploys, proposes to the timelock, and is the Vault's guardian, as the deployment's
    /// multisig is.
    deployer: Address,
    timelock: Address,
    miner: JoinHandle<anyhow::Result<()>>,
    _anvil: ChildProcess,
}

impl Drop for Chain {
    /// Stops block production with the chain it mines.
    fn drop(&mut self) {
        self.miner.abort();
    }
}

impl Chain {
    /// Starts an isolated chain mined once a second and deploys token, actual OZ timelock and Vault.
    /// Error cases:
    /// - Anvil, compiled artifacts, RPC, or a deployment is unavailable
    pub async fn start(dir: &Path) -> anyhow::Result<Self> {
        let port = free_port()?;
        let mut anvil = ChildProcess::spawn(
            "anvil",
            &[
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--chain-id",
                "31337",
                "--no-mining",
                "--slots-in-an-epoch",
                "1",
                "--silent",
            ],
            &dir.join("anvil.log"),
            &[],
        )?;
        let mut cfg = dev_config();
        cfg.tee.use_published_mock_key = true;
        let key = EnclaveKey::for_config(&cfg.tee)?;
        let (url, client) =
            (format!("http://127.0.0.1:{port}"), reqwest::Client::builder().timeout(LOADED_WAIT).build()?);
        timeout(LOADED_WAIT, async {
            while rpc::<String>(&client, &url, "eth_chainId", json!([])).await.is_err() {
                anvil.check()?;
                sleep(Duration::from_millis(40)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("local Anvil startup")??;
        let mining = Arc::new(RwLock::new(()));
        let mut chain = Self {
            miner: tokio::spawn(mine(client.clone(), url.clone(), mining.clone())),
            url,
            token: Address::ZERO,
            vault: Address::ZERO,
            relayer: Address::ZERO,
            payer: Address::ZERO,
            funder: Address::ZERO,
            deployer: Address::ZERO,
            timelock: Address::ZERO,
            signer: engine_core::types::AccountId(key.address()),
            mining,
            client,
            _anvil: anvil,
        };
        let accounts: Vec<Address> = chain.request("eth_accounts", json!([])).await?;
        let deployer = accounts[0];
        chain.deployer = deployer;
        chain.payer = accounts[7];
        chain.funder = accounts[8];
        chain.relayer = accounts[9];
        chain.token = chain.deploy(deployer, "MockUSDC.sol/MockUSDC.json", &[]).await?;
        chain.timelock = chain
            .deploy(
                deployer,
                "TimelockController.sol/TimelockController.json",
                &(U256::from(1), vec![deployer], vec![Address::ZERO], Address::ZERO).abi_encode_params(),
            )
            .await?;
        chain.vault = chain
            .deploy(
                deployer,
                "Vault.sol/Vault.json",
                // The MONKEY period budget, 1 MONKEY, is a development value.
                &(
                    chain.token,
                    Address::from(key.address()),
                    chain.timelock,
                    deployer,
                    U256::from(10).pow(U256::from(18)),
                )
                    .abi_encode(),
            )
            .await?;
        // The node's custody startup reads at the withdrawal finality tag, which trails `safe`.
        timeout(LOADED_WAIT, async {
            while chain.request::<String>("eth_getCode", json!([chain.vault, "finalized"])).await? == "0x" {
                sleep(Duration::from_millis(100)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("Vault deployment reaching the finalized block")??;
        Ok(chain)
    }

    /// Calls only the freshly started loopback chain under a per-request deadline.
    /// Error cases:
    /// - transport, HTTP, JSON-RPC, or response decoding fails
    pub async fn request<T: DeserializeOwned>(&self, method: &str, params: Value) -> anyhow::Result<T> {
        rpc(&self.client, &self.url, method, params).await
    }

    /// Fails if Anvil exited or block production stopped.
    /// Error cases:
    /// - the Anvil process or the miner task has finished
    pub fn check(&mut self) -> anyhow::Result<()> {
        self._anvil.check()?;
        ensure!(!self.miner.is_finished(), "local block production stopped");
        Ok(())
    }

    /// Sends a real local transaction and waits for its mined receipt.
    /// Error cases:
    /// - submission, receipt polling, or the inclusion deadline fails
    async fn transact(&self, from: Address, to: Option<Address>, data: Vec<u8>) -> anyhow::Result<TransactionReceipt> {
        let mut transaction = json!({"from":from,"data":format!("0x{}",hex::encode(data)),"gas":"0xb71b00"});
        if let Some(to) = to {
            transaction["to"] = json!(to);
        }
        let hash: B256 = self.request("eth_sendTransaction", json!([transaction])).await?;
        timeout(LOADED_WAIT, async {
            loop {
                if let Some(receipt) =
                    self.request::<Option<TransactionReceipt>>("eth_getTransactionReceipt", json!([hash])).await?
                {
                    return Ok(receipt);
                }
                sleep(Duration::from_millis(40)).await;
            }
        })
        .await
        .context("local transaction inclusion")?
    }

    /// Deploys compiled creation bytecode with actual ABI constructor arguments.
    /// Error cases:
    /// - the artifact is absent/malformed or contract creation reverts
    async fn deploy(&self, from: Address, artifact: &str, args: &[u8]) -> anyhow::Result<Address> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out").join(artifact);
        let value: Value = serde_json::from_slice(
            &std::fs::read(&path).with_context(|| format!("forge build first: {}", path.display()))?,
        )?;
        let mut code =
            hex::decode(value["bytecode"]["object"].as_str().context("creation bytecode")?.trim_start_matches("0x"))?;
        code.extend_from_slice(args);
        let receipt = self.transact(from, None, code).await?;
        ensure!(receipt.status(), "local deployment reverted: {artifact}");
        receipt.contract_address.context("creation receipt omitted its address")
    }

    /// Executes a token or Vault call and requires successful EVM execution.
    /// Error cases:
    /// - transaction submission or execution fails
    pub async fn send(&self, from: Address, to: Address, data: Vec<u8>) -> anyhow::Result<TransactionReceipt> {
        let receipt = self.transact(from, Some(to), data).await?;
        ensure!(receipt.status(), "local contract transaction reverted");
        Ok(receipt)
    }

    /// The Vault's guardian.
    pub fn guardian(&self) -> Address {
        self.deployer
    }

    /// Runs a Vault call through the timelock: its proposer schedules it, and it executes once the
    /// one-second delay has passed.
    /// Error cases:
    /// - scheduling fails, or execution still reverts at the deadline
    pub async fn govern(&self, data: Vec<u8>) -> anyhow::Result<()> {
        let (target, value, predecessor, salt) = (self.vault, U256::ZERO, B256::ZERO, B256::ZERO);
        let delay = U256::from(1);
        let schedule = scheduleCall { target, value, data: data.clone().into(), predecessor, salt, delay };
        self.send(self.deployer, self.timelock, schedule.abi_encode()).await?;
        let execute = executeCall { target, value, payload: data.into(), predecessor, salt }.abi_encode();
        timeout(LOADED_WAIT, async {
            // Ready only in a block at least the delay later; one mines every second.
            while !self.transact(self.deployer, Some(self.timelock), execute.clone()).await?.status() {
                sleep(Duration::from_millis(200)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("timelock operation execution")?
    }

    /// Reads a typed ABI value at latest; production canonical-finality reads run inside the node.
    /// Error cases:
    /// - RPC or ABI decoding fails
    pub async fn read<C: SolCall>(&self, to: Address, call: C) -> anyhow::Result<C::Return> {
        let value: String = self
            .request("eth_call", json!([{"to":to,"data":format!("0x{}",hex::encode(call.abi_encode()))},"latest"]))
            .await?;
        Ok(C::abi_decode_returns(&hex::decode(value.trim_start_matches("0x"))?)?)
    }

    /// Funds a controlled test user and deposits through actual Vault transactions.
    /// Error cases:
    /// - local funding, token allowance, or a real deposit fails
    pub async fn deposit(&self, account: Address, units: u64, house: bool) -> anyhow::Result<()> {
        let _: Value = self.request("anvil_impersonateAccount", json!([account])).await?;
        let _: Value = self.request("anvil_setBalance", json!([account, "0xde0b6b3a7640000"])).await?;
        let amount = U256::from(units) * U256::from(1_000_000);
        self.send(self.deployer, self.token, mintCall { to: account, amount }.abi_encode()).await?;
        self.send(account, self.token, approveCall { spender: self.vault, amount }.abi_encode()).await?;
        let input =
            if house { depositToHouseCall { amount }.abi_encode() } else { depositCall { amount }.abi_encode() };
        self.send(account, self.vault, input).await?;
        Ok(())
    }

    /// Checks that malformed or replayed claims revert without sending any transaction.
    /// Error cases:
    /// - an invalid call unexpectedly succeeds
    pub async fn refuses(&self, input: Vec<u8>) -> anyhow::Result<()> {
        let response: Value = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"eth_call",
            "params":[{"from":self.payer,"to":self.vault,"data":format!("0x{}",hex::encode(input))},"latest"]}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            response["error"]["message"].as_str().is_some_and(|text| text.contains("revert")),
            "invalid claim did not return an EVM revert: {response}"
        );
        Ok(())
    }

    /// Submits the duplicate claim on-chain and requires its mined receipt to revert.
    /// Error cases:
    /// - submission fails or the replay unexpectedly executes successfully
    pub async fn duplicate_reverts(&self, input: Vec<u8>) -> anyhow::Result<()> {
        let receipt = self.transact(self.payer, Some(self.vault), input).await?;
        ensure!(
            !receipt.status() && receipt.logs().is_empty(),
            "duplicate payment transaction did not revert atomically"
        );
        Ok(())
    }

    /// Encodes the authorization received from the actual public WebSocket stream.
    /// Error cases:
    /// - typed authorization, approval sequence, or signature is malformed
    pub fn claim(&self, signed: &SignedWithdrawal) -> anyhow::Result<withdrawCall> {
        crate::withdrawals::verify(
            signed,
            CheckpointDomain { chain_id: 31337, vault: engine_core::types::AccountId(self.vault.into()) },
            self.signer,
        )?;
        let auth = signed.authorization.as_ref().context("stream authorization")?;
        Ok(withdrawCall {
            withdrawal: Withdrawal {
                account: Address::from(signed.account.0),
                amount: crate::withdrawals::token_units(signed.amount)?,
                nonce: signed.nonce,
                approvedSeq: signed.approved_seq.context("approval seq")?,
                signerEpoch: auth.signer_epoch,
                issuedAt: auth.issued_at,
                expiresAt: auth.expires_at,
            },
            checkpointSeq: auth.checkpoint_seq,
            siblings: auth.siblings.clone(),
            signature: hex::decode(signed.signature.trim_start_matches("0x"))?.into(),
        })
    }
}

/// Sends one JSON-RPC request to the loopback chain.
/// Error cases:
/// - transport, HTTP, JSON-RPC, or response decoding fails
async fn rpc<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
) -> anyhow::Result<T> {
    let response: Value = client
        .post(url)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    ensure!(response.get("error").is_none(), "local RPC {method}: {}", response["error"]);
    Ok(serde_json::from_value(response.get("result").context("RPC result missing")?.clone())?)
}

/// Mines one block a second while no proxied node request is in flight. While Anvil mines, two
/// reads pinned to the latest block hash can see different states, which the node would take as a
/// conflicting checkpoint or collateral read.
/// Error cases:
/// - Anvil refuses or stops answering a mining request
async fn mine(client: reqwest::Client, url: String, mining: Arc<RwLock<()>>) -> anyhow::Result<()> {
    let mut tick = interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let _mining = mining.write().await;
        rpc::<Value>(&client, &url, "evm_mine", json!([])).await.context("mining a local block")?;
    }
}
