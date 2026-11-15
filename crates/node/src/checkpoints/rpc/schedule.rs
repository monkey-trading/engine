//! The daily turn the vault's registry's schedule names at a time, read at a finalized block, as
//! `contracts/src/OperatorSchedule.sol` declares it.

use alloy::eips::BlockId;
use alloy::primitives::Address;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};
use engine_core::types::{AccountId, Millis};

use crate::checkpoints::rpc::{candidateOfCall, registryCall};
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
        address source;
    }
    function turnAt(uint64 timestamp) external view returns (uint64 turn, TurnReturn memory);
    function rotationState() external view returns (bool enabled, bytes32 release, uint64 generation, uint64 until);
    function turnOf(uint64 turn) external view returns (TurnReturn memory);
    function setOf(uint32 setId)
        external
        view
        returns (uint64 epoch, uint64 effectiveAt, address[] memory candidates, uint256[] memory weights);
    function memberAt(uint32 setId, bytes32 seed, uint64 turn, uint16 position) external view returns (address);
    function MAX_DRAW_POSITIONS() external view returns (uint16);
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
        let boundary = engine_core::turns::boundary(turn).context("the turn's boundary is out of range")?;
        // Once the turn is over it still names its member, unless the registry has since installed
        // an operator, which ends the turns: at the block, the turn that started last is this one,
        // and the registry names no turn and no generation within it. A turn the block has yet to
        // reach, as at a boundary finality trails, is not ended by anything the block shows.
        let raw = self.call(registry, rotationStateCall {}.abi_encode(), block).await?;
        let state = rotationStateCall::abi_decode_returns(&raw).context("decoding the registry's rotation state")?;
        let ends = u64::try_from(boundary + engine_core::turns::DAY_MS).ok().map(|ms| ms / 1000);
        if Some(state.until) == ends && state.generation == 0 {
            return Ok(None);
        }
        let member = self.candidacy(registry, drawn.candidate, block).await?;
        // The holder the registry named when the turn was drawn, which hands the member the keys.
        let source = match Some(drawn.source).filter(|source| !source.is_zero() && *source != drawn.candidate) {
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
            set: drawn.setId,
            source,
        }))
    }

    /// See `CheckpointChain::turn_order`: the schedule's `memberAt` for each position a draw can reach,
    /// its weighted positions and then its scan, stopping once every member of the set appeared.
    /// Error cases:
    /// - the observation is not finalized and pinned, or a call or its decoding fails
    pub(super) async fn read_turn_order(&self, at: &Observation, turn: u64) -> anyhow::Result<Vec<HandoverSource>> {
        ensure!(at.view == View::Finalized, "the schedule requires a finalized observation");
        let evidence = at.lifecycle.as_ref().context("the schedule requires a pinned canonical observation")?;
        let block = BlockId::hash_canonical(evidence.block_hash);
        let vault = Address::from(self.domain.vault.0);
        let registry = registryCall::abi_decode_returns(&self.call(vault, registryCall {}.abi_encode(), block).await?)?;
        ensure!(!registry.is_zero(), "the vault follows no operator registry");
        let schedule =
            scheduleCall::abi_decode_returns(&self.call(registry, scheduleCall {}.abi_encode(), block).await?)?;
        ensure!(!schedule.is_zero(), "the registry names no schedule");
        let drawn =
            turnOfCall::abi_decode_returns(&self.call(schedule, turnOfCall { turn }.abi_encode(), block).await?)
                .context("decoding the schedule's turn")?;
        if drawn.candidate.is_zero() {
            return Ok(Vec::new());
        }
        let raw = self.call(schedule, setOfCall { setId: drawn.setId }.abi_encode(), block).await?;
        let members = setOfCall::abi_decode_returns(&raw).context("decoding the schedule's set")?.candidates.len();
        let raw = self.call(schedule, MAX_DRAW_POSITIONSCall {}.abi_encode(), block).await?;
        let weighted = MAX_DRAW_POSITIONSCall::abi_decode_returns(&raw).context("decoding the draw's positions")?;
        let mut order: Vec<Address> = Vec::new();
        for position in 0..weighted.saturating_add(u16::try_from(members).unwrap_or(u16::MAX)) {
            if order.len() == members {
                break;
            }
            let call = memberAtCall { setId: drawn.setId, seed: drawn.seed, turn, position };
            let member = memberAtCall::abi_decode_returns(&self.call(schedule, call.abi_encode(), block).await?)
                .context("decoding the schedule's member")?;
            if !order.contains(&member) {
                order.push(member);
            }
        }
        let mut sources = Vec::with_capacity(order.len());
        for member in order {
            sources.push(self.handover_source_of(registry, member, block).await?);
        }
        Ok(sources)
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
