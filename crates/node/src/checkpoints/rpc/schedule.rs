//! The daily turn the vault's registry's schedule names at a time, read at a finalized block, as
//! `contracts/src/OperatorSchedule.sol` declares it.

use alloy::eips::BlockId;
use alloy::primitives::Address;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};
use engine_core::types::{AccountId, Millis};

use crate::checkpoints::rpc::{candidateOfCall, operatorInEffectCall, registryCall};
use crate::checkpoints::{CheckpointRpc, HandoverSource, Observation, ScheduledTurn, View};

sol! {
    function schedule() external view returns (address);
    struct TurnReturn {
        address candidate;
        uint16 position;
        uint8 reason;
        uint32 setId;
        uint64 generation;
        uint64 prior;
        bytes32 seed;
    }
    function turnAt(uint64 timestamp) external view returns (uint64 turn, TurnReturn memory);
    function sourceOf(uint64 turn) external view returns (address);
}

impl CheckpointRpc {
    /// See `CheckpointChain::scheduled_turn`.
    /// Error cases:
    /// - the observation is not finalized and pinned, `now_ms` is negative, or a call or its
    ///   decoding fails
    pub(super) async fn read_scheduled_turn(
        &self,
        at: &Observation,
        now_ms: Millis,
    ) -> anyhow::Result<Option<ScheduledTurn>> {
        ensure!(at.view == View::Finalized, "the schedule requires a finalized observation");
        let evidence = at.lifecycle.as_ref().context("the schedule requires a pinned canonical observation")?;
        let block = BlockId::hash_canonical(evidence.block_hash);
        let vault = Address::from(self.domain.vault.0);
        let registry = registryCall::abi_decode_returns(&self.call(vault, registryCall {}.abi_encode(), block).await?)?;
        if registry.is_zero() {
            return Ok(None);
        }
        let schedule =
            scheduleCall::abi_decode_returns(&self.call(registry, scheduleCall {}.abi_encode(), block).await?)?;
        if schedule.is_zero() {
            return Ok(None);
        }
        let timestamp = u64::try_from(now_ms / 1000).context("a time before the epoch")?;
        let raw = self.call(schedule, turnAtCall { timestamp }.abi_encode(), block).await?;
        let found = turnAtCall::abi_decode_returns(&raw).context("decoding the schedule's turn")?;
        let (turn, drawn) = (found.turn, found._1);
        if drawn.candidate.is_zero() {
            return Ok(None);
        }
        let member = self.candidacy(registry, drawn.candidate, block).await?;
        let boundary = engine_core::turns::boundary(turn).context("the turn's boundary is out of range")?;
        let source = match source_of(self, schedule, turn, block).await? {
            Some(source) => Some(source),
            // The first turn takes over from the operator the registry named before any turn.
            None => {
                let raw = self.call(registry, operatorInEffectCall {}.abi_encode(), block).await?;
                Some(operatorInEffectCall::abi_decode_returns(&raw)?.candidate).filter(|named| !named.is_zero())
            }
        };
        let source = match source.filter(|source| *source != drawn.candidate) {
            Some(source) => Some(self.handover_source_of(registry, source, block).await?),
            None => None,
        };
        Ok(Some(ScheduledTurn {
            turn,
            boundary,
            candidate: AccountId(drawn.candidate.0.0),
            admin_key: AccountId(member.adminKey.0.0),
            release: member.release.0,
            generation: drawn.generation,
            source,
        }))
    }

    /// `candidate`'s registration at `block`.
    /// Error cases:
    /// - the call or its decoding fails
    async fn candidacy(
        &self,
        registry: Address,
        candidate: Address,
        block: BlockId,
    ) -> anyhow::Result<crate::checkpoints::rpc::CandidateReturn> {
        let raw = self.call(registry, candidateOfCall { candidate }.abi_encode(), block).await?;
        candidateOfCall::abi_decode_returns(&raw).context("decoding the registry's candidate")
    }

    /// `candidate`'s handover data, as its candidacy registered it at `block`.
    /// Error cases:
    /// - the call or its decoding fails
    async fn handover_source_of(
        &self,
        registry: Address,
        candidate: Address,
        block: BlockId,
    ) -> anyhow::Result<HandoverSource> {
        let registered = self.candidacy(registry, candidate, block).await?;
        Ok(HandoverSource {
            candidate: AccountId(candidate.0.0),
            release: registered.release.0,
            api_hostname: registered.apiHostname,
            handover_endpoint: registered.handoverEndpoint,
        })
    }
}

/// The member of the drawn turn before `turn`, if any.
/// Error cases:
/// - the call or its decoding fails
async fn source_of(
    rpc: &CheckpointRpc,
    schedule: Address,
    turn: u64,
    block: BlockId,
) -> anyhow::Result<Option<Address>> {
    let raw = rpc.call(schedule, sourceOfCall { turn }.abi_encode(), block).await?;
    Ok(Some(sourceOfCall::abi_decode_returns(&raw)?).filter(|source| !source.is_zero()))
}
