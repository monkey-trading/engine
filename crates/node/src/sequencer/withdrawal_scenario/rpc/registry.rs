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
        let reason = String::from_utf8_lossy(decoded.get(4..).unwrap_or(&[]));
        Ok(if reason.chars().any(|c| !c.is_ascii_graphic() && c != ' ') {
            format!("0x{}", hex::encode(&decoded))
        } else {
            reason.into_owned()
        })
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
    /// vault to it and it to the vault, and turns rotation on when `rotation`; returns the staking
    /// contract the registry deployed. Test parameters only; #360 decides production values.
    /// Error cases:
    /// - a deployment or governance call fails
    pub(crate) async fn deploy_registry(
        &mut self,
        admin_key: Address,
        launch_digest: [u8; 48],
        config_hash: [u8; 32],
        liveness: u64,
        rotation: bool,
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
        if rotation {
            self.govern_target(self.registry, setRotationCall { enabled: true }.abi_encode()).await?;
        }
        let staking: Address = self.read(self.registry, stakingCall {}).await?;
        Ok(staking)
    }

    /// Warps to just past the open epoch's end and closes it, returning the elected pending
    /// election, if any. The warp is `evm_setNextBlockTimestamp`, so the close's block carries
    /// `at_secs`.
    /// Error cases:
    /// - the warp or the close reverts
    pub(crate) async fn close_epoch_at(&self, at_secs: u64) -> anyhow::Result<Option<(Address, u64)>> {
        {
            let _mining = self.mining.write().await;
            self.request::<Value>("evm_setNextBlockTimestamp", json!([at_secs])).await?;
            self.request::<Value>("evm_mine", json!([])).await?;
        }
        let receipt = self
            .transact(self.payer, Some(self.registry), closeEpochCall {}.abi_encode())
            .await
            .context("closing the election epoch")?;
        ensure!(receipt.status(), "closing the election epoch reverted");
        let pending = self.read(self.registry, pendingElectionCall {}).await?;
        Ok((pending.effectiveAt != 0).then_some((pending.candidate, pending.effectiveAt)))
    }

    /// The registry's open epoch's start, seconds.
    pub(crate) async fn open_epoch_start(&self) -> anyhow::Result<u64> {
        Ok(self.read(self.registry, openEpochStartCall {}).await?)
    }

    /// Folds the registry-driven release change into the vault's generation, as the promoter does
    /// after an election's effective time.
    /// Error cases:
    /// - the transaction reverts
    pub(crate) async fn sync_release(&self) -> anyhow::Result<()> {
        let receipt = self
            .transact(self.payer, Some(self.vault), syncReleaseCall {}.abi_encode())
            .await
            .context("syncRelease")?;
        ensure!(receipt.status(), "syncRelease reverted");
        Ok(())
    }

    /// Mints MONKEY to `account` as the vault, its only minter, stakes it, and registers `account`
    /// as a candidate with `admin_key`'s authorization, the self-bond `bond`, and the release
    /// `launch_digest` under `config_hash`, whose approval the timelock then grants.
    /// Error cases:
    /// - a local transaction or the registration's authorization fails
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn register_candidate(
        &self,
        account: Address,
        admin_key: &k256::ecdsa::SigningKey,
        bond: U256,
        launch_digest: [u8; 48],
        config_hash: [u8; 32],
        enclave: Address,
        api_hostname: &str,
        handover_endpoint: &str,
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
            apiHostname: api_hostname.into(),
            handoverEndpoint: handover_endpoint.into(),
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

    /// Sends `to` with `data` from `from` while Anvil impersonates it, so a candidate whose
    /// account is not one of the unlocked development accounts can still act on chain.
    /// Error cases:
    /// - the impersonation or the transaction fails
    pub(crate) async fn impersonate_send(&self, from: Address, to: Address, data: Vec<u8>) -> anyhow::Result<()> {
        let _: Value = self.request("anvil_impersonateAccount", json!([from])).await?;
        let _: Value = self.request("anvil_setBalance", json!([from, "0xde0b6b3a7640000"])).await?;
        let sent = self.send(from, to, data).await;
        let _: Value = self.request("anvil_stopImpersonatingAccount", json!([from])).await?;
        sent.map(|_| ())
    }

    /// Mints MONKEY to `account` as the vault and stakes `amount` of it, so a non-candidate
    /// account can delegate.
    /// Error cases:
    /// - a local transaction fails
    pub(crate) async fn stake_from(&self, account: Address, amount: U256) -> anyhow::Result<()> {
        let monkey: Address = self.read(self.vault, monkeyCall {}).await?;
        let minter: Address = self.read(monkey, minterCall {}).await?;
        println!("STAKE_FROM probe: vault={:#x} monkey={:#x} minter={:#x}", self.vault, monkey, minter);
        let _: Value = self.request("anvil_impersonateAccount", json!([self.vault])).await?;
        let _: Value = self.request("anvil_setBalance", json!([self.vault, "0xde0b6b3a7640000"])).await?;
        self.send(self.vault, monkey, mintCall { to: account, amount }.abi_encode()).await?;
        let _: Value = self.request("anvil_stopImpersonatingAccount", json!([self.vault])).await?;
        let staking: Address = self.read(self.registry, stakingCall {}).await?;
        self.send(account, monkey, approveCall { spender: staking, amount }.abi_encode()).await?;
        self.send(account, staking, stakeCall { amount }.abi_encode()).await?;
        Ok(())
    }

    /// `account` delegates all its stake to `candidate`.
    /// Error cases:
    /// - the local transaction fails
    pub(crate) async fn delegate_from(&self, account: Address, candidate: Address) -> anyhow::Result<()> {
        let staking: Address = self.read(self.registry, stakingCall {}).await?;
        self.send(account, staking, delegateCall { candidate }.abi_encode()).await?;
        Ok(())
    }

    /// Warps the next block to `at_secs` and mines it.
    /// Error cases:
    /// - the time warp or mining fails
    pub(crate) async fn warp_to(&self, at_secs: u64) -> anyhow::Result<()> {
        let _mining = self.mining.write().await;
        self.request::<Value>("evm_setNextBlockTimestamp", json!([at_secs])).await?;
        self.request::<Value>("evm_mine", json!([])).await?;
        Ok(())
    }

    /// Mints through the vault in place of a proven claim, stakes, and registers `account` as a
    /// candidate with `admin_key`'s authorization, the self-bond `bond`, the release
    /// `launch_digest` under `config_hash`, and the handover data, as `account` itself — the
    /// pattern the operator in effect follows, whose candidate identity is its admin key's
    /// address, which is not one of the unlocked development accounts.
    /// Error cases:
    /// - a local transaction, the registration's authorization, or the approval fails
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn register_impersonated_candidate(
        &self,
        account: Address,
        admin_key: &k256::ecdsa::SigningKey,
        bond: U256,
        launch_digest: [u8; 48],
        config_hash: [u8; 32],
        enclave: Address,
        api_hostname: &str,
        handover_endpoint: &str,
    ) -> anyhow::Result<()> {
        let monkey: Address = self.read(self.vault, monkeyCall {}).await?;
        let staking: Address = self.read(self.registry, stakingCall {}).await?;
        let _: Value = self.request("anvil_impersonateAccount", json!([self.vault])).await?;
        let _: Value = self.request("anvil_setBalance", json!([self.vault, "0xde0b6b3a7640000"])).await?;
        self.send(self.vault, monkey, mintCall { to: account, amount: bond }.abi_encode()).await?;
        let _: Value = self.request("anvil_stopImpersonatingAccount", json!([self.vault])).await?;
        let admin = Address::from(engine_core::eip712::address(admin_key.verifying_key()).0);
        let registration = CandidateRegistration {
            adminKey: admin,
            enclave,
            bond,
            launchDigest: launch_digest.to_vec().into(),
            configHash: config_hash.into(),
            apiHostname: api_hostname.into(),
            handoverEndpoint: handover_endpoint.into(),
        };
        let digest: B256 =
            self.read(self.registry, registrationDigestCall { candidate: account, adminKey: admin }).await?;
        let signature = engine_core::eip712::sign(admin_key, &digest);
        let register = registerCall { registration, signature: signature.to_vec().into() }.abi_encode();
        self.impersonate_send(account, monkey, approveCall { spender: staking, amount: bond }.abi_encode()).await?;
        self.impersonate_send(account, staking, stakeCall { amount: bond }.abi_encode()).await?;
        self.impersonate_send(account, self.registry, register).await?;
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
