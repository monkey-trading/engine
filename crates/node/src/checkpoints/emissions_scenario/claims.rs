//! The scenario's on-chain claims: proofs read from the API, claimed against the checkpoint they
//! name, and the signer changes that end them.

use std::time::Duration;

use alloy::primitives::{B256, Bytes, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use axum::http::StatusCode;
use engine_core::command::Command;
use engine_core::merkle::EntitlementProof;
use engine_core::types::AccountId;
use serde_json::json;
use tokio::time::sleep;

use crate::checkpoints::emissions_scenario::{Scenario, claimEmissionsCall, latestRootCall, latestSeqCall};
use crate::checkpoints::scenario::send;

sol! {
    function retireSigner() external;
    function setSigner(address signer) external;
    function schedule(address target, uint256 value, bytes data, bytes32 predecessor, bytes32 salt, uint256 delay) external;
    function execute(address target, uint256 value, bytes payload, bytes32 predecessor, bytes32 salt) external payable;
}

impl Scenario {
    /// Sequences one more command and waits for the checkpoint after it to post.
    pub(super) async fn later_checkpoint(&self) {
        self.submit(Command::Tick).await;
        self.checkpoint_posted().await;
    }

    /// Sequences commands until the vault posts a checkpoint past `after`, which the node withholds
    /// until it has read the installed signer.
    pub(super) async fn first_checkpoint_after(&self, after: u64) {
        crate::test_support::wait_for("the installed signer's first checkpoint to post", async {
            while self.view(self.vault, latestSeqCall {}).await <= after {
                self.submit(Command::Tick).await;
                sleep(Duration::from_millis(40)).await;
            }
        })
        .await;
    }

    /// Fetches `account`'s entitlement proof from the API once the vault has posted its checkpoint,
    /// and has the relayer claim it; returns the MONKEY minted.
    pub(super) async fn claim(&self, account: AccountId) -> U256 {
        let proof = self.posted_proof(account).await;
        self.claim_proof(&proof).await
    }

    /// Has the relayer claim `proof`; returns the MONKEY minted.
    pub(super) async fn claim_proof(&self, proof: &EntitlementProof) -> U256 {
        let before = self.balance(proof.account).await;
        send(&self.provider, self.relayer, self.vault, claim_call(proof)).await;
        assert_eq!(self.balance(AccountId(self.relayer.0.0)).await, U256::ZERO, "the relayer receives nothing");
        self.balance(proof.account).await - before
    }

    /// The selector the vault reverts with when the relayer claims `account`'s current proof.
    pub(super) async fn refused_claim(&self, account: AccountId) -> [u8; 4] {
        let proof = self.posted_proof(account).await;
        self.refused_proof(&proof).await
    }

    /// The selector the vault reverts with when the relayer claims `proof`.
    pub(super) async fn refused_proof(&self, proof: &EntitlementProof) -> [u8; 4] {
        let call = TransactionRequest::default().from(self.relayer).to(self.vault).input(claim_call(proof).into());
        let error = self.provider.request::<_, Bytes>("eth_call", (call, "latest")).await.unwrap_err();
        let data = error.as_error_resp().and_then(|payload| payload.as_revert_data()).expect("a revert");
        data[..4].try_into().unwrap()
    }

    /// `account`'s proof from the API against the root the vault holds.
    pub(super) async fn posted_proof(&self, account: AccountId) -> EntitlementProof {
        self.checkpoint_posted().await;
        let (status, body) = self.get(&format!("/v1/accounts/{account}/emissions")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let proof = serde_json::from_value::<crate::sequencer::ClaimAnswer<EntitlementProof>>(body).unwrap().proof;
        assert_eq!(proof.root, self.view(self.vault, latestRootCall {}).await, "the proof is for the posted root");
        proof
    }

    /// The guardian retires the signer.
    pub(super) async fn retire_signer(&self) {
        send(&self.provider, self.funder, self.vault, retireSignerCall {}.abi_encode()).await;
    }

    /// Waits for the API to say nothing of `account`'s is claimable, as the worker's next read of the
    /// vault tells it.
    pub(super) async fn claims_closed(&self, account: AccountId) {
        crate::test_support::wait_for("the node to read the vault's change of signer", async {
            loop {
                let (status, body) = self.get(&format!("/v1/accounts/{account}/emissions")).await;
                assert_eq!(status, StatusCode::OK, "{body}");
                if body["claimable"] == json!(false) {
                    assert_eq!(body["accepted_seq"], json!(null));
                    return;
                }
                sleep(Duration::from_millis(40)).await;
            }
        })
        .await;
    }

    /// The timelock installs the node's own key in a new signer epoch.
    pub(super) async fn install_signer(&self) {
        let payload = setSignerCall { signer: self.key.address().into() }.abi_encode();
        let schedule = scheduleCall {
            target: self.vault,
            value: U256::ZERO,
            data: payload.clone().into(),
            predecessor: B256::ZERO,
            salt: B256::ZERO,
            delay: U256::from(1),
        };
        send(&self.provider, self.funder, self.governance, schedule.abi_encode()).await;
        let _: serde_json::Value = self.provider.request("evm_increaseTime", json!([2])).await.unwrap();
        let execute = executeCall {
            target: self.vault,
            value: U256::ZERO,
            payload: payload.into(),
            predecessor: B256::ZERO,
            salt: B256::ZERO,
        };
        send(&self.provider, self.funder, self.governance, execute.abi_encode()).await;
    }
}

/// The vault call that claims `proof`.
fn claim_call(proof: &EntitlementProof) -> Vec<u8> {
    let siblings: Vec<B256> = proof.siblings.clone();
    claimEmissionsCall {
        account: proof.account.0.into(),
        entitled: U256::from(proof.entitled.0),
        checkpointSeq: proof.seq,
        siblings,
    }
    .abi_encode()
}
