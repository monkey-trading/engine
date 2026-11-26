//! The schedule's governance record (`contracts/src/GovernanceRecord.sol`): the vote and fault proof
//! contracts the registry's timelock recorded for the schedule's nodes to mirror, read through the
//! record the schedule created.

use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::transports::RpcError;
use anyhow::Context;

use crate::custody::evm::{BlockRef, GovernanceRecord};
use crate::custody::rpc::VaultRpc;

sol! {
    function governanceRecord() external view returns (address);
    function vote() external view returns (address);
    function faultProof() external view returns (address);
}

impl VaultRpc {
    /// The contracts the configured schedule's governance record names, each read in the canonical block
    /// `at`: `Absent` where the schedule reverts `governanceRecord()`, as one deployed before schedules
    /// made a record does, and `Unset` where the schedule has no code there or the record's vote contract
    /// is still zero.
    /// Error cases:
    /// - no schedule is configured, a call fails or times out, or an answer does not decode as an address
    pub(super) async fn read_governance_record(&self, at: BlockRef) -> anyhow::Result<GovernanceRecord> {
        let schedule = self.schedule.context("no schedule is configured")?;
        let block = BlockId::hash_canonical(at.hash);
        let request = TransactionRequest::default().to(schedule).input(governanceRecordCall {}.abi_encode().into());
        let answer = self
            .deadline("eth_call", async {
                match self.client.request::<_, Bytes>("eth_call", (request, block)).await {
                    Ok(out) => Ok(Some(out)),
                    // A contract with no such function and no fallback reverts, with no data.
                    Err(RpcError::ErrorResp(payload)) if payload.message.contains("revert") => Ok(None),
                    Err(error) => Err(error),
                }
            })
            .await
            .context("reading the schedule's governance record")?;
        let Some(out) = answer else { return Ok(GovernanceRecord::Absent) };
        if out.is_empty() {
            return Ok(GovernanceRecord::Unset);
        }
        let record =
            governanceRecordCall::abi_decode_returns(&out).context("decoding the schedule's governance record")?;
        let out = self.record_call(record, voteCall {}.abi_encode(), block).await?;
        let vote = voteCall::abi_decode_returns(&out).context("decoding the record's vote contract")?;
        let out = self.record_call(record, faultProofCall {}.abi_encode(), block).await?;
        let fault_proof =
            faultProofCall::abi_decode_returns(&out).context("decoding the record's fault proof contract")?;
        Ok(if vote.is_zero() { GovernanceRecord::Unset } else { GovernanceRecord::Set { vote, fault_proof } })
    }

    /// The record's answer to `input` in `block`.
    /// Error cases:
    /// - the call fails, reverts or times out
    async fn record_call(&self, record: Address, input: Vec<u8>, block: BlockId) -> anyhow::Result<Bytes> {
        let request = TransactionRequest::default().to(record).input(input.into());
        self.deadline("eth_call", self.client.request("eth_call", (request, block)))
            .await
            .context("reading the schedule's governance record")
    }
}
