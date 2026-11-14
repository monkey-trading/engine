//! The schedule the rotation scenarios deploy, with the LogHeads companion its heartbeat rule reads
//! (#595), and the standby keys a close adopts only members with.

use alloy::primitives::{Address, B256, Bytes};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};

use super::chain::Chain;
use crate::checkpoints::scenario::deploy;
use crate::tee::EnclaveKey;

sol! {
    function vouchDigest(address candidate, address key, bytes32 release, uint64 nonce, uint64 expiry)
        external
        view
        returns (bytes32);
    function registerStandby(address key, uint64 nonce, uint64 expiry, bytes vouch) external;
}

/// The heartbeat rule the scenarios deploy with, as the forge tests' defaults: 60 s slots, 30 s
/// tolerance, 5 misses, 200 blocks; its bounds; the switch budget and the pinned block time.
const LIVENESS: (u64, u64, u16, u16) = (60, 30, 5, 200);
const MINIMUMS: (u64, u64, u16, u16) = (30, 5, 2, 10);
const MAXIMUMS: (u64, u64, u16, u16) = (120, 60, 9, 256);
const SWITCH_BUDGET_SECS: u64 = 120;
const BLOCK_MILLIS: u64 = 250;

impl Chain {
    /// Deploys the LogHeads companion of the vault and the schedule over the seed `receiver`, and
    /// returns both.
    pub(super) async fn deploy_schedule(&self, receiver: Address) -> (Address, Address) {
        self.deploy_schedule_with(receiver, LIVENESS, SWITCH_BUDGET_SECS).await
    }

    /// `deploy_schedule` with the heartbeat rule `rule` and the switch budget `switch_budget`, in
    /// seconds.
    pub(super) async fn deploy_schedule_with(
        &self,
        receiver: Address,
        rule: (u64, u64, u16, u16),
        switch_budget: u64,
    ) -> (Address, Address) {
        let from = self.accounts[0];
        let log_heads =
            deploy(&self.provider, from, "LogHeads.sol/LogHeads.json", &(self.vault,).abi_encode_params()).await;
        let args = (self.registry, receiver, log_heads, rule, MINIMUMS, MAXIMUMS, switch_budget, BLOCK_MILLIS);
        let schedule =
            deploy(&self.provider, from, "OperatorSchedule.sol/OperatorSchedule.json", &args.abi_encode_params()).await;
        (schedule, log_heads)
    }

    /// Registers a fresh standby key for each of `members` (candidate and its release's id), vouched
    /// by the vault's signer `signer`, from each candidate's own account.
    pub(super) async fn register_standbys(&self, members: &[(Address, B256)], signer: &EnclaveKey) {
        for &(candidate, release) in members {
            let key = Address::from(EnclaveKey::generate().address());
            self.register_standby(candidate, release, key, signer).await;
        }
    }

    /// Registers `key` as `candidate`'s standby key for its release `release`, vouched by `signer`, from
    /// the candidate's own account.
    pub(super) async fn register_standby(&self, candidate: Address, release: B256, key: Address, signer: &EnclaveKey) {
        let schedule = self.schedule.expect("the schedule is deployed");
        let expiry = self.latest_timestamp().await + 3_600;
        let digest = self.view(schedule, vouchDigestCall { candidate, key, release, nonce: 1, expiry }).await;
        let vouch: Bytes = signer.sign_digest(digest.0).parse().expect("65 bytes of hex");
        let call = registerStandbyCall { key, nonce: 1, expiry, vouch }.abi_encode();
        let request =
            alloy::rpc::types::TransactionRequest::default().from(candidate).to(schedule).input(call.clone().into());
        if let Err(e) = self.provider.request::<_, Bytes>("eth_call", (request, "latest")).await {
            panic!("registerStandby for {candidate} would revert: {e}");
        }
        self.send(candidate, schedule, call).await;
    }
}
