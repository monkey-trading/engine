//! On-chain steps of the staking scenario: claiming MONKEY entitlements and USDC distributions
//! against posted checkpoint roots, mirroring staking events into the engine, and reading balances
//! back.

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::{Log, TransactionReceipt, TransactionRequest};
use alloy::sol_types::SolCall;
use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::merkle::{DistributionProof, EntitlementProof};
use engine_core::staking::UsdcUnits;
use engine_core::types::AccountId;

use super::{balanceOfCall, latestRootCall};
use crate::checkpoints::scenario::transact;
use crate::checkpoints::staking_scenario::{claimDistributionCall, claimEmissionsCall};

use super::Scenario;

impl Scenario {
    /// Fetches `account`'s entitlement proof from the API once the vault has posted its checkpoint,
    /// and has the relayer claim it; returns the MONKEY minted.
    pub(super) async fn claim_emissions(&self, account: AccountId) -> U256 {
        self.checkpoint_posted().await;
        let (status, body) = self.get(&format!("/v1/accounts/{account}/emissions")).await;
        assert_eq!(status, hyper::StatusCode::OK, "{body}");
        let proof: EntitlementProof = serde_json::from_value(body).unwrap();
        assert_eq!(proof.root, self.view(self.vault, latestRootCall {}).await, "the proof is for the posted root");
        let before = self.monkey_balance(account).await;
        self.send_tx(self.relayer, self.vault, emissions_call(&proof)).await;
        self.monkey_balance(account).await - before
    }

    /// Fetches `account`'s distribution proof from the API once the vault has posted its
    /// checkpoint, and has the relayer claim it; returns the USDC paid.
    pub(super) async fn claim_distribution(&self, account: AccountId) -> U256 {
        let proof = self.posted_distribution_proof(account).await;
        let before = self.usdc_balance(proof.account.0.into()).await;
        self.send_tx(self.relayer, self.vault, distribution_call(&proof)).await;
        assert_eq!(self.usdc_balance(self.relayer).await, U256::ZERO, "the relayer receives nothing");
        self.usdc_balance(proof.account.0.into()).await - before
    }

    /// The selector the vault reverts with when the relayer claims `account`'s current proof.
    pub(super) async fn refused_distribution_claim(&self, account: AccountId) -> [u8; 4] {
        let proof = self.posted_distribution_proof(account).await;
        let call =
            TransactionRequest::default().from(self.relayer).to(self.vault).input(distribution_call(&proof).into());
        let error = self.provider.request::<_, Bytes>("eth_call", (call, "latest")).await.unwrap_err();
        let data = error.as_error_resp().and_then(|payload| payload.as_revert_data()).expect("a revert");
        data[..4].try_into().unwrap()
    }

    /// `account`'s distribution proof from the API against the root the vault holds.
    async fn posted_distribution_proof(&self, account: AccountId) -> DistributionProof {
        self.checkpoint_posted().await;
        let (status, body) = self.get(&format!("/v1/accounts/{account}/distributions")).await;
        assert_eq!(status, hyper::StatusCode::OK, "{body}");
        let proof: DistributionProof = serde_json::from_value(body).unwrap();
        assert_eq!(proof.root, self.view(self.vault, latestRootCall {}).await, "the proof is for the posted root");
        proof
    }

    /// Mirrors a stake transaction's `Staked` event into the engine under its chain reference.
    pub(super) async fn mirror_stake(&self, receipt: TransactionReceipt, account: AccountId, amount: TokenUnits) {
        let (chain_ref, block) = self.chain_ref(&receipt).await;
        let events = self.submit(Command::Stake { account, amount, block, chain_ref }).await;
        assert!(matches!(events[..], [Event::Staked { .. }]), "{events:?}");
    }

    /// Mirrors an unstake transaction's `UnstakeCompleted` event into the engine under its chain
    /// reference.
    pub(super) async fn mirror_unstake(&self, receipt: TransactionReceipt, account: AccountId, amount: TokenUnits) {
        let (chain_ref, block) = self.chain_ref(&receipt).await;
        let events = self.submit(Command::Unstake { account, amount, block, chain_ref }).await;
        assert!(matches!(events[..], [Event::Unstaked { .. }]), "{events:?}");
    }

    /// The engine's chain reference for the staking transaction's single event log, and the block
    /// it finalised in.
    async fn chain_ref(&self, receipt: &TransactionReceipt) -> (String, u64) {
        let log = self.event_log(receipt);
        (
            format!("{}:{}", receipt.transaction_hash, log.log_index.expect("a mined log has an index")),
            receipt.block_number.expect("a mined receipt has a block"),
        )
    }

    /// The single log a staking transaction emits, from the staking contract.
    fn event_log<'a>(&self, receipt: &'a TransactionReceipt) -> &'a Log {
        let logs: Vec<_> = receipt.logs().iter().filter(|log| log.address() == self.staking).collect();
        assert_eq!(logs.len(), 1, "a staking transaction emits exactly one event");
        logs[0]
    }

    /// Sends a transaction from a local Anvil account and returns its receipt.
    pub(super) async fn transact(&self, from: Address, to: Address, input: Vec<u8>) -> TransactionReceipt {
        transact(&self.provider, TransactionRequest::default().from(from).to(to).input(input.into())).await
    }

    /// Sends a transaction from a local Anvil account, asserting it succeeds.
    pub(super) async fn send_tx(&self, from: Address, to: Address, input: Vec<u8>) {
        let receipt = self.transact(from, to, input).await;
        assert!(receipt.status(), "transaction reverted");
    }

    /// One Anvil control RPC, asserted to succeed.
    pub(super) async fn local_rpc(&self, method: &str, params: serde_json::Value) {
        let response: serde_json::Value = reqwest::Client::new()
            .post(&self.url)
            .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(response.get("error").is_none(), "{response}");
    }

    /// The engine's staking state.
    pub(super) async fn staking_state(&self) -> Option<engine_core::staking::Staking> {
        self.query(|e| e.staking().cloned()).await
    }

    /// Every staked balance, as the replayed engine must also hold it.
    pub(super) async fn staked_balances(&self) -> std::collections::BTreeMap<AccountId, TokenUnits> {
        self.query(|e| e.staked_balances().clone()).await
    }

    /// `account`'s staked MONKEY.
    pub(super) async fn staked(&self, account: AccountId) -> TokenUnits {
        self.query(move |e| e.staked(account)).await
    }

    /// `account`'s cumulative USDC distribution.
    pub(super) async fn distribution(&self, account: AccountId) -> UsdcUnits {
        self.query(move |e| e.distribution(account)).await
    }

    /// House NAV now.
    pub(super) async fn house_nav(&self) -> Fixed {
        self.query(|e| e.house_nav()).await
    }

    /// Cumulative custody liabilities the vault must hold, in USDC base units: genesis house
    /// capital plus the dev deposits credited so far.
    pub(super) fn liabilities(&self) -> U256 {
        U256::from(((self.genesis.house_capital + self.deposits).raw() / 1_000) as u64)
    }

    /// `account`'s MONKEY balance.
    pub(super) async fn monkey_balance(&self, account: AccountId) -> U256 {
        self.view(self.monkey, balanceOfCall { account: account.0.into() }).await
    }

    /// `account`'s USDC balance.
    pub(super) async fn usdc_balance(&self, account: Address) -> U256 {
        self.view(self.usdc, balanceOfCall { account }).await
    }

    /// Calls a view function at `to`.
    pub(super) async fn view<C: SolCall>(&self, to: Address, call: C) -> C::Return {
        let request = TransactionRequest::default().to(to).input(call.abi_encode().into());
        let out: Bytes = self.provider.request("eth_call", (request, "latest")).await.unwrap();
        C::abi_decode_returns(&out).unwrap()
    }
}

/// The vault call that claims `proof`.
fn emissions_call(proof: &EntitlementProof) -> Vec<u8> {
    let siblings: Vec<B256> = proof.siblings.clone();
    claimEmissionsCall { account: proof.account.0.into(), entitled: U256::from(proof.entitled.0), siblings }
        .abi_encode()
}

/// The vault call that claims `proof`.
fn distribution_call(proof: &DistributionProof) -> Vec<u8> {
    let siblings: Vec<B256> = proof.siblings.clone();
    claimDistributionCall { account: proof.account.0.into(), entitled: U256::from(proof.entitled.0), siblings }
        .abi_encode()
}
