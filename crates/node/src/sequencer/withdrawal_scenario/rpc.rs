//! Local unlocked Anvil transactions and the same Vault ABI used by real clients.

use std::path::Path;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::TransactionReceipt;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use anyhow::{Context, ensure};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

use crate::log::CheckpointDomain;
use crate::sequencer::SignedWithdrawal;
use crate::sequencer::withdrawal_scenario::node::{ChildProcess, free_port};
use crate::tee::EnclaveKey;
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
    client: reqwest::Client,
    deployer: Address,
    _anvil: ChildProcess,
}

impl Chain {
    /// Starts an isolated auto-mining chain and deploys token, actual OZ timelock and Vault.
    /// Error cases:
    /// - Anvil, compiled artifacts, RPC, or a deployment is unavailable
    pub async fn start(dir: &Path) -> anyhow::Result<Self> {
        let port = free_port()?;
        let anvil = ChildProcess::spawn(
            "anvil",
            &[
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--chain-id",
                "31337",
                "--block-time",
                "1",
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
        let mut chain = Self {
            url: format!("http://127.0.0.1:{port}"),
            token: Address::ZERO,
            vault: Address::ZERO,
            relayer: Address::ZERO,
            payer: Address::ZERO,
            funder: Address::ZERO,
            deployer: Address::ZERO,
            signer: engine_core::types::AccountId(key.address()),
            client: reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?,
            _anvil: anvil,
        };
        timeout(Duration::from_secs(10), async {
            while chain.request::<String>("eth_chainId", json!([])).await.is_err() {
                chain._anvil.check()?;
                sleep(Duration::from_millis(40)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("local Anvil startup")??;
        let accounts: Vec<Address> = chain.request("eth_accounts", json!([])).await?;
        let deployer = accounts[0];
        chain.deployer = deployer;
        chain.payer = accounts[7];
        chain.funder = accounts[8];
        chain.relayer = accounts[9];
        chain.token = chain.deploy(deployer, "MockUSDC.sol/MockUSDC.json", &[]).await?;
        let timelock = chain
            .deploy(
                deployer,
                "TimelockController.sol/TimelockController.json",
                &(U256::from(1), vec![deployer], vec![Address::ZERO], Address::ZERO).abi_encode(),
            )
            .await?;
        chain.vault = chain
            .deploy(
                deployer,
                "Vault.sol/Vault.json",
                &(chain.token, Address::from(key.address()), timelock).abi_encode(),
            )
            .await?;
        Ok(chain)
    }

    /// Calls only the freshly started loopback chain under a per-request deadline.
    /// Error cases:
    /// - transport, HTTP, JSON-RPC, or response decoding fails
    pub async fn request<T: DeserializeOwned>(&self, method: &str, params: Value) -> anyhow::Result<T> {
        let response: Value = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(response.get("error").is_none(), "local RPC {method}: {}", response["error"]);
        Ok(serde_json::from_value(response.get("result").context("RPC result missing")?.clone())?)
    }

    /// Sends a real local transaction and waits for its mined receipt.
    /// Error cases:
    /// - submission, receipt polling, or the ten-second inclusion deadline fails
    async fn transact(&self, from: Address, to: Option<Address>, data: Vec<u8>) -> anyhow::Result<TransactionReceipt> {
        let mut transaction = json!({"from":from,"data":format!("0x{}",hex::encode(data)),"gas":"0xb71b00"});
        if let Some(to) = to {
            transaction["to"] = json!(to);
        }
        let hash: B256 = self.request("eth_sendTransaction", json!([transaction])).await?;
        timeout(Duration::from_secs(10), async {
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
