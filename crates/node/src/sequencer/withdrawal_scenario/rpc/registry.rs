//! The scenario chain's operator-registry support: deployment, candidacy, promotion, and the
//! liveness clock's time warps.

use std::time::Duration;

use alloy::primitives::{B256, U256};
use alloy::sol_types::SolCall;
use anyhow::Context;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

use super::*;
use crate::test_support::LOADED_WAIT;

impl Chain {
    /// The revert reason of a call from the payer, if it reverts with data.
    /// Error cases:
    /// - the simulation itself fails
    pub(crate) async fn revert_reason(&self, to: Address, input: &[u8]) -> anyhow::Result<String> {
        let response: Value = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"eth_call",
            "params":[{"from":self.payer,"to":to,"data":format!("0x{}",hex::encode(input))},"latest"]}))
            .send()
            .await?
            .json()
            .await?;
        let data = response["error"]["data"].as_str().unwrap_or_default();
        let decoded = hex::decode(data.strip_prefix("0x").unwrap_or(data)).unwrap_or_default();
        Ok(String::from_utf8_lossy(decoded.get(4..).unwrap_or(&[])).into_owned())
    }


    /// Has the timelock call `target` with `data`.
    /// Error cases:
    /// - scheduling fails, or execution keeps reverting past the deadline
    pub(crate) async fn govern_target(&self, target: Address, data: Vec<u8>) -> anyhow::Result<()> {
        let salt = B256::from(rand::random::<[u8; 32]>());
        let (value, predecessor) = (U256::ZERO, B256::ZERO);
        let scheduled =
            scheduleCall { target, value, data: data.clone().into(), predecessor, salt, delay: U256::from(1) };
        self.send(self.deployer, self.timelock, scheduled.abi_encode()).await?;
        let execute = executeCall { target, value, payload: data.into(), predecessor, salt }.abi_encode();
        timeout(LOADED_WAIT, async {
            while !self.transact(self.payer, Some(self.timelock), execute.clone()).await?.status() {
                sleep(Duration::from_millis(250)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("the timelock never executed the call")?
    }

    /// Deploys the operator registry with `admin_key` the initial operator in effect, names the
    /// vault to it and it to the vault, and turns rotation on; returns the staking contract the
    /// registry deployed. Test parameters only; #360 decides production values.
    /// Error cases:
    /// - a deployment or governance call fails
    pub(crate) async fn deploy_registry(
        &mut self,
        admin_key: Address,
        launch_digest: [u8; 48],
        config_hash: [u8; 32],
        liveness: u64,
    ) -> anyhow::Result<Address> {
        let initial = InitialOperator {
            adminKey: admin_key,
            launchDigest: launch_digest.to_vec().into(),
            configHash: config_hash.into(),
        };
        let parameters = RegistryParameters {
            epochLength: 3_600,
            minTerm: 0,
            finalityMargin: 60,
            marginBps: 1_000,
            eligibilityBps: 100,
            quorumBps: 2_000,
            maxCandidates: 10,
            minSelfBond: U256::from(1_000),
            livenessTimeout: liveness,
        };
        let (lo, hi) = (
            RegistryParameters {
                epochLength: 60,
                minTerm: 0,
                finalityMargin: 0,
                marginBps: 0,
                eligibilityBps: 0,
                quorumBps: 0,
                maxCandidates: 1,
                minSelfBond: U256::from(1_000),
                livenessTimeout: liveness,
            },
            RegistryParameters {
                epochLength: 86_400,
                minTerm: 86_400,
                finalityMargin: 3_600,
                marginBps: 10_000,
                eligibilityBps: 10_000,
                quorumBps: 10_000,
                maxCandidates: 100,
                minSelfBond: U256::from(1_000_000),
                livenessTimeout: 3_600,
            },
        );
        let monkey: Address = self.read(self.vault, monkeyCall {}).await?;
        let args = (
            monkey,
            U256::from(crate::test_support::DEV_EXIT_SCHEDULE.1),
            U256::from(30 * 60),
            self.timelock,
            initial,
            parameters,
            lo,
            hi,
        )
            .abi_encode_params();
        self.registry = self.deploy(self.deployer, "OperatorRegistry.sol/OperatorRegistry.json", &args).await?;
        self.govern_target(self.registry, setVaultCall { vault: self.vault }.abi_encode()).await?;
        self.govern(setRegistryCall { registry_: self.registry }.abi_encode()).await?;
        self.govern_target(self.registry, setRotationCall { enabled: true }.abi_encode()).await?;
        let staking: Address = self.read(self.registry, stakingCall {}).await?;
        Ok(staking)
    }

    /// Mints MONKEY to `account` as the vault, its only minter, stakes it, and registers `account`
    /// as a candidate with `admin_key`'s authorization, the self-bond `bond`, and the release
    /// `launch_digest` under `config_hash`, whose approval the timelock then grants.
    /// Error cases:
    /// - a local transaction or the registration's authorization fails
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn register_candidate(
        &self,
        account: Address,
        admin_key: &k256::ecdsa::SigningKey,
        bond: U256,
        launch_digest: [u8; 48],
        config_hash: [u8; 32],
        enclave: Address,
    ) -> anyhow::Result<()> {
        let monkey: Address = self.read(self.vault, monkeyCall {}).await?;
        let _: Value = self.request("anvil_impersonateAccount", json!([self.vault])).await?;
        let _: Value = self.request("anvil_setBalance", json!([self.vault, "0xde0b6b3a7640000"])).await?;
        self.send(self.vault, monkey, mintCall { to: account, amount: bond }.abi_encode()).await?;
        let _: Value = self.request("anvil_stopImpersonatingAccount", json!([self.vault])).await?;
        let staking: Address = self.read(self.registry, stakingCall {}).await?;
        self.send(account, monkey, approveCall { spender: staking, amount: bond }.abi_encode()).await?;
        self.send(account, staking, stakeCall { amount: bond }.abi_encode()).await?;
        let admin = Address::from(engine_core::eip712::address(admin_key.verifying_key()).0);
        let registration = CandidateRegistration {
            adminKey: admin,
            enclave,
            bond,
            launchDigest: launch_digest.to_vec().into(),
            configHash: config_hash.into(),
            apiHostname: "api.standby.local".into(),
            handoverEndpoint: "127.0.0.1:9444".into(),
        };
        let digest: B256 =
            self.read(self.registry, registrationDigestCall { candidate: account, adminKey: admin }).await?;
        let signature = engine_core::eip712::sign(admin_key, &digest);
        self.send(
            account,
            self.registry,
            registerCall { registration, signature: signature.to_vec().into() }.abi_encode(),
        )
        .await?;
        self.govern_target(
            self.registry,
            setReleaseApprovalCall {
                candidate: account,
                launchDigest: launch_digest.to_vec().into(),
                configHash: config_hash.into(),
                approved: true,
            }
            .abi_encode(),
        )
        .await?;
        Ok(())
    }

    /// The candidate's self-delegation, which makes it the standby.
    /// Error cases:
    /// - the local transaction fails
    pub(crate) async fn delegate_self(&self, account: Address) -> anyhow::Result<()> {
        let staking: Address = self.read(self.registry, stakingCall {}).await?;
        self.send(account, staking, delegateCall { candidate: account }.abi_encode()).await?;
        Ok(())
    }

    /// Promotes the top standby after the liveness timeout, folding the release change into the
    /// vault's state: anyone may call both, and the scenario's `payer` does.
    /// Error cases:
    /// - promotion or the sync reverts
    pub(crate) async fn promote(&self) -> anyhow::Result<()> {
        self.send(self.payer, self.registry, promoteCall {}.abi_encode()).await?;
        self.send(self.payer, self.vault, syncReleaseCall {}.abi_encode()).await?;
        Ok(())
    }

    /// Moves the next block at least `seconds` past the vault's last checkpoint, so the liveness
    /// timeout has lapsed.
    /// Error cases:
    /// - the time warp or mining fails
    pub(crate) async fn lapse_liveness(&self, seconds: u64) -> anyhow::Result<()> {
        let _mining = self.mining.write().await;
        let block: Value = self.request("eth_getBlockByNumber", json!(["latest", false])).await?;
        let now = u64::from_str_radix(block["timestamp"].as_str().context("timestamp")?.trim_start_matches("0x"), 16)?;
        self.request::<Value>("evm_setNextBlockTimestamp", json!([now + seconds])).await?;
        self.request::<Value>("evm_mine", json!([])).await?;
        Ok(())
    }

    /// Checks that a call to `to` from the payer reverts without sending any transaction.
    /// Error cases:
    /// - the call unexpectedly succeeds
    pub(crate) async fn refuses_call(&self, to: Address, input: Vec<u8>) -> anyhow::Result<()> {
        let response: Value = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"eth_call",
            "params":[{"from":self.payer,"to":to,"data":format!("0x{}",hex::encode(input))},"latest"]}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            response["error"]["message"].as_str().is_some_and(|text| text.contains("revert")),
            "invalid call did not return an EVM revert: {response}"
        );
        Ok(())
    }
}
