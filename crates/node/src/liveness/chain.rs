//! What the liveness rule reads from the chain, every call at one block a strict majority of the
//! custody providers agrees on (`standby_key::chain::AgreedBlock`), and the calls a liveness caller
//! sends: the vault's signer, the registry's rotation state, the schedule's heartbeat rule, marks,
//! turn and promotion time, and the epoch's stored head, as `contracts/src/` declares them.

use alloy::primitives::{Address, B256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::Context;
use engine_core::types::AccountId;
use futures_util::future::BoxFuture;

use crate::standby_key::chain::{
    AgreedBlock, StandbyDomain, candidateOfCall, checkpointSignerCall, promotableCall, signerEpochCall, turnAtCall,
};

sol! {
    function rotationState() external view returns (bool enabled, bytes32 release, uint64 generation, uint64 until);
    function liveness()
        external
        view
        returns (uint64 heartbeatInterval, uint64 headTolerance, uint16 heartbeatMisses, uint16 headBlocks);
    function latestMark() external view returns (uint64 slot, uint64 blockNumber);
    function promotionDueAt() external view returns (uint64);
    function setOf(uint32 setId)
        external
        view
        returns (uint64 epoch, uint64 effectiveAt, address[] candidates, uint256[] weights);
    function heads(uint64 epoch)
        external
        view
        returns (uint64 seq, bytes32 link, uint64 signedAt, uint64 boundBlock, uint64 postedAt);
    function keyHolder(address adminKey) external view returns (address candidate);
    function registerStandby(address key, uint64 nonce, uint64 expiry, bytes vouch) external;
    function witness() external returns (bool marked);
    function promote() external returns (uint64 turn, uint16 position);
}

/// The schedule's heartbeat rule, in its own units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rule {
    /// Slot length, in seconds.
    pub interval: u64,
    /// How far a live head's signing time may be from its block's, in seconds.
    pub tolerance: u64,
    /// How many marked slots with no head a promotion needs.
    pub misses: u16,
    /// How many recent blocks a held head may bind.
    pub blocks: u16,
}

/// The newest head `LogHeads` holds for a signer epoch; zeros before its first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StoredHead {
    pub seq: u64,
    pub link: B256,
    /// A v2 head's signing time, in milliseconds; zero for v1.
    pub signed_at: u64,
    /// A held head's bound block, zero otherwise.
    pub bound_block: u64,
    /// The block time it was posted at, in seconds.
    pub posted_at: u64,
}

/// The turn in effect at a block's time and its latest assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Named {
    pub turn: u64,
    pub candidate: AccountId,
    pub position: u16,
    pub reason: u8,
    pub set: u32,
    pub generation: u64,
}

/// What the liveness rule depends on, at one agreed block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct View {
    /// The block's time, in seconds, number and hash.
    pub time: u64,
    pub number: u64,
    pub hash: B256,
    /// The vault's signer epoch and signer.
    pub epoch: u64,
    pub signer: AccountId,
    /// The release and generation the registry names for the operator in effect; zero generation
    /// while no turn of the schedule names the operator.
    pub release: B256,
    pub generation: u64,
    pub rule: Rule,
    /// `promotionDueAt`: the earliest block time any promotion could pass, in seconds.
    pub due_at: u64,
    /// The latest marked slot, and the Arbitrum block that marked it.
    pub marked_slot: u64,
    pub marked_block: u64,
    /// The turn in effect, `None` before any is drawn.
    pub named: Option<Named>,
    /// The signer epoch's stored head.
    pub head: StoredHead,
}

/// A rotation-set member as the liveness rule reaches it: where its node answers, and the standby key
/// it signs confirmations with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Member {
    pub candidate: AccountId,
    /// Its registered handover endpoint, `host:port`, empty when it registered none.
    pub endpoint: String,
    /// Its promotable standby key, `None` while it has none.
    pub standby: Option<AccountId>,
}

/// The contracts the liveness rule reads: the standby keys' domain and the LogHeads contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LivenessDomain {
    pub standby: StandbyDomain,
    pub log_heads: Address,
}

impl Rule {
    /// The slot length.
    pub(crate) fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.interval)
    }
}

impl View {
    /// Whether a turn of the schedule names the operator: the registry's operator generation is set.
    pub(crate) fn turn_names_operator(&self) -> bool {
        self.generation != 0
    }
}

/// The view at `block` in `domain`.
/// Error cases:
/// - a call fails, or its return does not decode
pub(crate) fn view<'a, B: AgreedBlock>(
    block: &'a B,
    domain: &'a LivenessDomain,
) -> BoxFuture<'a, anyhow::Result<View>> {
    Box::pin(read_view(block, domain))
}

/// `view`, unboxed.
/// Error cases:
/// - as `view`
async fn read_view(block: &impl AgreedBlock, domain: &LivenessDomain) -> anyhow::Result<View> {
    let (vault, registry, schedule) = (domain.standby.vault, domain.standby.registry, domain.standby.schedule);
    let epoch = call(block, "liveness_signer_epoch", vault, signerEpochCall {}).await?;
    let signer = call(block, "liveness_signer", vault, checkpointSignerCall {}).await?;
    let rotation = call(block, "liveness_rotation", registry, rotationStateCall {}).await?;
    let rule = call(block, "liveness_rule", schedule, livenessCall {}).await?;
    let due_at = call(block, "liveness_due_at", schedule, promotionDueAtCall {}).await?;
    let mark = call(block, "liveness_mark", schedule, latestMarkCall {}).await?;
    let found = call(block, "liveness_turn", schedule, turnAtCall { timestamp: block.time() }).await?;
    let stored = call(block, "liveness_head", domain.log_heads, headsCall { epoch }).await?;
    let named = (found.turn != 0).then_some(Named {
        turn: found.turn,
        candidate: AccountId(found._1.candidate.0.0),
        position: found._1.position,
        reason: found._1.reason,
        set: found._1.setId,
        generation: found._1.generation,
    });
    Ok(View {
        time: block.time(),
        number: block.number(),
        hash: block.hash(),
        epoch,
        signer: AccountId(signer.0.0),
        release: rotation.release,
        generation: rotation.generation,
        rule: Rule {
            interval: rule.heartbeatInterval,
            tolerance: rule.headTolerance,
            misses: rule.heartbeatMisses,
            blocks: rule.headBlocks,
        },
        due_at,
        marked_slot: mark.slot,
        marked_block: mark.blockNumber,
        named,
        head: StoredHead {
            seq: stored.seq,
            link: stored.link,
            signed_at: stored.signedAt,
            bound_block: stored.boundBlock,
            posted_at: stored.postedAt,
        },
    })
}

/// The members of set `set` at `block`, each with its registered handover endpoint and promotable
/// standby key.
/// Error cases:
/// - a call fails, or its return does not decode
pub(crate) fn members<'a, B: AgreedBlock>(
    block: &'a B,
    domain: &'a StandbyDomain,
    set: u32,
) -> BoxFuture<'a, anyhow::Result<Vec<Member>>> {
    Box::pin(read_members(block, domain, set))
}

/// `members`, unboxed.
/// Error cases:
/// - as `members`
async fn read_members(block: &impl AgreedBlock, domain: &StandbyDomain, set: u32) -> anyhow::Result<Vec<Member>> {
    let listed = call(block, "liveness_set", domain.schedule, setOfCall { setId: set }).await?;
    let mut members = Vec::with_capacity(listed.candidates.len());
    for candidate in listed.candidates {
        let registered = call(block, "liveness_candidate", domain.registry, candidateOfCall { candidate }).await?;
        let key = call(block, "liveness_promotable", domain.schedule, promotableCall { candidate }).await?;
        members.push(Member {
            candidate: AccountId(candidate.0.0),
            endpoint: registered.handoverEndpoint,
            standby: (!key.is_zero()).then_some(AccountId(key.0.0)),
        });
    }
    Ok(members)
}

/// The set the turn in effect at `block`'s time was drawn from, `None` before any is drawn.
/// Error cases:
/// - the call fails, or its return does not decode
pub(crate) fn turn_set<'a, B: AgreedBlock>(
    block: &'a B,
    domain: &'a LivenessDomain,
) -> BoxFuture<'a, anyhow::Result<Option<u32>>> {
    Box::pin(async move {
        let found =
            call(block, "liveness_turn", domain.standby.schedule, turnAtCall { timestamp: block.time() }).await?;
        Ok((found.turn != 0).then_some(found._1.setId))
    })
}

/// The candidate that registered `admin` as its admin key at `block`, `None` while none has.
/// Error cases:
/// - the call fails, or its return does not decode
pub(crate) fn candidate_of<'a, B: AgreedBlock>(
    block: &'a B,
    domain: &'a StandbyDomain,
    admin: AccountId,
) -> BoxFuture<'a, anyhow::Result<Option<AccountId>>> {
    Box::pin(async move {
        let holder =
            call(block, "liveness_key_holder", domain.registry, keyHolderCall { adminKey: Address::from(admin.0) })
                .await?;
        Ok((!holder.is_zero()).then_some(AccountId(holder.0.0)))
    })
}

/// `candidate`'s promotable standby key at `block`, `None` while it has none.
/// Error cases:
/// - the call fails, or its return does not decode
pub(crate) fn promotable<'a, B: AgreedBlock>(
    block: &'a B,
    domain: &'a StandbyDomain,
    candidate: AccountId,
) -> BoxFuture<'a, anyhow::Result<Option<AccountId>>> {
    Box::pin(async move {
        let key = call(
            block,
            "liveness_promotable",
            domain.schedule,
            promotableCall { candidate: Address::from(candidate.0) },
        )
        .await?;
        Ok((!key.is_zero()).then_some(AccountId(key.0.0)))
    })
}

/// `request` made at `block` under `operation`, its return decoded.
/// Error cases:
/// - the call fails, or its return does not decode
async fn call<C: SolCall>(
    block: &impl AgreedBlock,
    operation: &'static str,
    to: Address,
    request: C,
) -> anyhow::Result<C::Return> {
    let out = block.call(operation, to, request.abi_encode().into()).await?;
    C::abi_decode_returns(&out).with_context(|| format!("decoding {operation}"))
}
