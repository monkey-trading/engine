//! The vote contract's open proposals (`contracts/src/ParameterVote.sol`), read with the operator list
//! at its final block for the console: `GET /v1/operators` serves them beside the candidates, and
//! marks them unavailable, raising `proposals_read_failed`, when they cannot be read. Ids
//! count up in the order proposals open, and no window passes the contract's 30 days, so the read
//! pages back from the newest until a proposal opened that long before the block, past which none is
//! open.

use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};
use engine_core::command::AdminCommand;
use engine_core::emissions::TokenUnits;
use engine_core::governance::decode_command;
use engine_core::types::{AccountId, Millis};
use serde::Serialize;

use crate::custody::evm::{BlockRef, VaultReader};
use crate::custody::operators::OperatorsReader;

sol! {
    struct Proposal {
        bytes32 commandHash;
        address proposer;
        uint32 setId;
        bool fast;
        bool passed;
        uint64 opensAt;
        uint64 closesAt;
        uint256 totalWeight;
        uint256 yesWeight;
    }
    struct ProposalView { uint256 id; Proposal proposal; bytes command; }
    function proposalCount() external view returns (uint256);
    function proposals(uint256 first, uint256 limit) external view returns (ProposalView[]);
}

/// Proposals per `proposals` call: a command is at most 32 KiB, so a page stays within a public
/// endpoint's return limits.
const PAGE: u64 = 8;
/// The quorum operation each page votes under, newest first; the pages a read takes at most.
const PAGE_OPERATIONS: [&str; 8] = [
    "vote_proposals_0",
    "vote_proposals_1",
    "vote_proposals_2",
    "vote_proposals_3",
    "vote_proposals_4",
    "vote_proposals_5",
    "vote_proposals_6",
    "vote_proposals_7",
];
/// The longest window either path may have: `ParameterVote.MAX_WINDOW`.
const MAX_WINDOW_SECS: u64 = 30 * 24 * 60 * 60;

/// The proposals still taking votes at the read's block.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenProposals {
    vote: AccountId,
    /// Newest first.
    open: Vec<OpenProposal>,
    /// Whether the pages read stopped before the proposals that could still be open.
    truncated: bool,
    /// Whether the read failed, so none are listed though some may be open.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    unavailable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct OpenProposal {
    id: u64,
    proposer: AccountId,
    set_id: u32,
    fast: bool,
    opens_at_ms: Millis,
    closes_at_ms: Millis,
    /// The set's total weight, in MONKEY base units, of which more than two thirds passes it.
    total_weight: TokenUnits,
    yes_weight: TokenUnits,
    command_hash: B256,
    /// The admin command the bytes are the canonical JSON of; `None` for bytes that are none, which
    /// the engine refuses if the proposal passes.
    command: Option<AdminCommand>,
}

impl<R: VaultReader + Sync> OperatorsReader<R> {
    /// The open proposals of the vote contract `vote` at `at`, whose time is `time_ms`.
    /// Error cases:
    /// - the providers lack a quorum for a call
    /// - a page is not the ids asked for, or a command does not hash to its proposal's hash
    /// - a count, weight or time out of range
    pub(super) async fn read_proposals(
        &self,
        vote: Address,
        at: BlockRef,
        time_ms: Millis,
    ) -> anyhow::Result<OpenProposals> {
        let input = proposalCountCall {}.abi_encode().into();
        let out = self.quorum.call_as("vote_count", at, vote, input).await?;
        // No code at the block: the contract is not deployed yet, so it holds no proposal.
        let count = if out.is_empty() {
            U256::ZERO
        } else {
            proposalCountCall::abi_decode_returns_validate(&out)
                .context("decoding the vote contract's proposal count")?
        };
        let mut next = u64::try_from(count).context("the proposal count exceeds u64")?;
        let time = u64::try_from(time_ms / 1000).context("the block's time is before the epoch")?;
        let mut open = Vec::new();
        for operation in PAGE_OPERATIONS {
            if next == 0 {
                break;
            }
            let first = next.saturating_sub(PAGE - 1).max(1);
            let call = proposalsCall { first: U256::from(first), limit: U256::from(next - first + 1) };
            let page = proposalsCall::abi_decode_returns_validate(
                &self.quorum.call_as(operation, at, vote, call.abi_encode().into()).await?,
            )
            .context("decoding a page of the vote contract's proposals")?;
            ensure!(
                page.len() as u64 == next - first + 1,
                "the vote contract returned {} proposals for ids {first} to {next}",
                page.len()
            );
            let mut older_closed = false;
            for (view, id) in page.iter().rev().zip((first..=next).rev()) {
                ensure!(view.id == U256::from(id), "the vote contract returned proposal {} for id {id}", view.id);
                let proposal = &view.proposal;
                ensure!(
                    keccak256(&view.command) == proposal.commandHash,
                    "proposal {id}'s command does not hash to its hash"
                );
                if !proposal.passed && proposal.closesAt > time {
                    open.push(OpenProposal::new(id, proposal, &view.command)?);
                }
                older_closed |= proposal.opensAt.saturating_add(MAX_WINDOW_SECS) <= time;
            }
            next = if older_closed { 0 } else { first - 1 };
        }
        Ok(OpenProposals { vote: AccountId(vote.into()), open, truncated: next != 0, unavailable: false })
    }
}

impl OpenProposals {
    /// What the list carries for `vote` when its proposals cannot be read.
    pub(super) fn unavailable(vote: Address) -> Self {
        Self { vote: AccountId(vote.into()), open: Vec::new(), truncated: false, unavailable: true }
    }
}

impl OpenProposal {
    /// Proposal `id` as the API shows it.
    /// Error cases:
    /// - a weight past `u128`, or a time past `i64` milliseconds
    fn new(id: u64, proposal: &Proposal, command: &[u8]) -> anyhow::Result<Self> {
        let units = |amount: U256| {
            crate::custody::staking::monkey_units(amount)
                .map(TokenUnits)
                .with_context(|| format!("weight {amount} exceeds u128"))
        };
        let millis = |seconds: u64| {
            i64::try_from(seconds)
                .ok()
                .and_then(|s| s.checked_mul(1000))
                .with_context(|| format!("time {seconds} is out of range"))
        };
        Ok(Self {
            id,
            proposer: AccountId(proposal.proposer.into()),
            set_id: proposal.setId,
            fast: proposal.fast,
            opens_at_ms: millis(proposal.opensAt)?,
            closes_at_ms: millis(proposal.closesAt)?,
            total_weight: units(proposal.totalWeight)?,
            yes_weight: units(proposal.yesWeight)?,
            command_hash: proposal.commandHash,
            command: decode_command(command),
        })
    }
}

#[cfg(test)]
pub(super) mod tests;
