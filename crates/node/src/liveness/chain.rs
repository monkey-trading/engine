//! What the liveness rule reads from the chain, every call at one block a strict majority of the
//! custody providers agrees on (`standby_key::chain::AgreedBlock`), and the calls a liveness caller
//! sends: the vault's signer, the registry's rotation state, the schedule's heartbeat rule, marks,
//! turn and promotion time, and the epoch's stored head, as `contracts/src/` declares them.

use alloy::primitives::{Address, B256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::Context;
use engine_core::types::AccountId;
use futures_util::future::{BoxFuture, try_join3};
use futures_util::{StreamExt, TryStreamExt, stream};

use crate::standby_key::chain::{
    AgreedBlock, StandbyDomain, candidateOfCall, checkpointSignerCall, heldTurnCall, promotableCall, signerEpochCall,
    standbyHolderCall, turnAtCall,
};

/// How many of a set's members are read at once.
const MEMBER_READS: usize = 8;
/// `OperatorRegistry.NOTICE`: a rotation set adopted now takes effect no sooner.
const SET_NOTICE_SECS: u64 = 7 * 86_400;
/// A turn is drawn from the set in effect at its draw deadline, a day before its boundary.
const DRAW_LEAD_SECS: u64 = 86_400;

sol! {
    function rotationState() external view returns (bool enabled, bytes32 release, uint64 generation, uint64 until);
    function liveness()
        external
        view
        returns (uint64 heartbeatInterval, uint64 headTolerance, uint16 heartbeatMisses, uint16 headBlocks);
    function latestMark() external view returns (uint64 slot, uint64 blockNumber);
    function promotionDueAt() external view returns (uint64);
    function currentSetId() external view returns (uint32);
    function pendingSetId() external view returns (uint32);
    function skipDueAt() external view returns (uint64);
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
    function skip() external returns (uint64 turn, uint16 position);
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
    /// The turn drawn before it, zero for the first.
    pub prior: u64,
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
    /// `skipDueAt`: the earliest block time the turn in effect's current assignment may be skipped, in
    /// seconds; `u64::MAX` once a head latched it (#596).
    pub skip_due_at: u64,
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
    /// The admin key its candidacy registered: the node that runs for it.
    pub admin_key: AccountId,
    /// Whether it held an assignment of the turn the members were read for, or of the turn drawn before
    /// it, whose holder keeps the keys until the turn's member takes them: the turn's first member, or
    /// one a promotion moved past, whose own tail past the promotion's head no member had to hold.
    pub held: bool,
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
    let skip_due_at = call(block, "liveness_skip_due_at", schedule, skipDueAtCall {}).await?;
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
        prior: found._1.prior,
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
        skip_due_at,
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

/// The first turn boundary, at or after the next one past `time`, a drawn turn could start at: one whose
/// draw deadline the earliest set to draw from has taken effect by, at `effective`; with no set in
/// effect or waiting (`None`), one adopted at `time` takes effect `SET_NOTICE_SECS` later at the earliest.
pub(crate) fn first_turnable(time: u64, effective: Option<u64>) -> u64 {
    let effective = effective.unwrap_or(time.saturating_add(SET_NOTICE_SECS));
    let boundary = crate::seed_relay::schedule::next_boundary;
    boundary(effective.saturating_add(DRAW_LEAD_SECS).saturating_sub(1)).max(boundary(time))
}

/// When the earliest set `schedule` could draw a turn from takes effect, at `block`: the set in effect,
/// else the one waiting; `None` with neither.
/// Error cases:
/// - a call fails, or its return does not decode
pub(crate) fn set_effective<B: AgreedBlock>(
    block: &B,
    schedule: Address,
) -> BoxFuture<'_, anyhow::Result<Option<u64>>> {
    Box::pin(async move {
        let current = call(block, "liveness_current_set", schedule, currentSetIdCall {}).await?;
        let set = match current {
            0 => call(block, "liveness_pending_set", schedule, pendingSetIdCall {}).await?,
            current => current,
        };
        if set == 0 {
            return Ok(None);
        }
        Ok(Some(call(block, "liveness_set", schedule, setOfCall { setId: set }).await?.effectiveAt))
    })
}

/// The key the vault names as its signer at `block`.
/// Error cases:
/// - the call fails, or its return does not decode
pub(crate) fn vault_signer<B: AgreedBlock>(block: &B, vault: Address) -> BoxFuture<'_, anyhow::Result<AccountId>> {
    Box::pin(async move {
        let signer = call(block, "liveness_signer", vault, checkpointSignerCall {}).await?;
        Ok(AccountId(signer.0.0))
    })
}

/// The candidate that registered `key` as a standby key with `schedule` at `block`, `None` for none.
/// Error cases:
/// - the call fails, or its return does not decode
pub(crate) fn standby_holder<B: AgreedBlock>(
    block: &B,
    schedule: Address,
    key: AccountId,
) -> BoxFuture<'_, anyhow::Result<Option<AccountId>>> {
    Box::pin(async move {
        let holder =
            call(block, "liveness_standby_holder", schedule, standbyHolderCall { key: Address::from(key.0) }).await?;
        Ok((!holder.is_zero()).then_some(AccountId(holder.0.0)))
    })
}

/// The newest head `log_heads` holds at `block` for signer epoch `epoch`.
/// Error cases:
/// - the call fails, or its return does not decode
pub(crate) fn stored_head<'a, B: AgreedBlock>(
    block: &'a B,
    log_heads: Address,
    epoch: u64,
) -> BoxFuture<'a, anyhow::Result<StoredHead>> {
    Box::pin(async move {
        let stored = call(block, "liveness_head", log_heads, headsCall { epoch }).await?;
        Ok(StoredHead {
            seq: stored.seq,
            link: stored.link,
            signed_at: stored.signedAt,
            bound_block: stored.boundBlock,
            posted_at: stored.postedAt,
        })
    })
}

/// The members of set `set` at `block`, each with its registered handover endpoint and promotable
/// standby key, and whether it held an assignment of `turn`, or of `prior`, the turn drawn before it,
/// whose holder keeps the keys into `turn` until its member takes them.
/// Error cases:
/// - a call fails, or its return does not decode
pub(crate) fn members<'a, B: AgreedBlock>(
    block: &'a B,
    domain: &'a StandbyDomain,
    (set, turn, prior): (u32, u64, u64),
) -> BoxFuture<'a, anyhow::Result<Vec<Member>>> {
    Box::pin(read_members(block, domain, set, (turn, prior)))
}

/// `members`, unboxed.
/// Error cases:
/// - as `members`
async fn read_members(
    block: &impl AgreedBlock,
    domain: &StandbyDomain,
    set: u32,
    (turn, prior): (u64, u64),
) -> anyhow::Result<Vec<Member>> {
    let listed = call(block, "liveness_set", domain.schedule, setOfCall { setId: set }).await?;
    let member = |candidate: Address| async move {
        let (registered, key, held) = try_join3(
            call(block, "liveness_candidate", domain.registry, candidateOfCall { candidate }),
            call(block, "liveness_promotable", domain.schedule, promotableCall { candidate }),
            call(block, "liveness_held", domain.schedule, heldTurnCall { turn, candidate }),
        )
        .await?;
        let held = held
            || (prior != 0
                && call(block, "liveness_held", domain.schedule, heldTurnCall { turn: prior, candidate }).await?);
        anyhow::Ok(Member {
            candidate: AccountId(candidate.0.0),
            endpoint: registered.handoverEndpoint,
            standby: (!key.is_zero()).then_some(AccountId(key.0.0)),
            admin_key: AccountId(registered.adminKey.0.0),
            held,
        })
    };
    // In the set's order, `MEMBER_READS` members at a time: a full set reads well within an interval.
    stream::iter(listed.candidates).map(member).buffered(MEMBER_READS).try_collect().await
}

/// The set the turn in effect at `block`'s time was drawn from, and the turn, `None` before any is
/// drawn.
/// Error cases:
/// - the call fails, or its return does not decode
pub(crate) fn turn_set<'a, B: AgreedBlock>(
    block: &'a B,
    domain: &'a LivenessDomain,
) -> BoxFuture<'a, anyhow::Result<Option<(u32, u64, u64)>>> {
    Box::pin(async move {
        let found =
            call(block, "liveness_turn", domain.standby.schedule, turnAtCall { timestamp: block.time() }).await?;
        Ok((found.turn != 0).then_some((found._1.setId, found.turn, found._1.prior)))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn can start only at a boundary whose draw deadline, a day before it, a set was in effect at:
    /// the next one for a set in effect by then, the one after for a set that takes effect later that
    /// day, and eight days on with none in effect or waiting.
    #[test]
    fn a_turn_can_start_only_at_a_boundary_a_set_was_in_effect_a_day_before() {
        // 08:00 UTC: the next boundary, 22:30, is 52,200 s on.
        let time = 1_800_000_000;
        let boundary = crate::seed_relay::schedule::next_boundary(time);
        assert_eq!(boundary - time, 52_200);
        assert_eq!(first_turnable(time, Some(0)), boundary, "a set in effect long since");
        assert_eq!(first_turnable(time, Some(boundary - DRAW_LEAD_SECS)), boundary, "in effect at the deadline");
        assert_eq!(first_turnable(time, Some(boundary - DRAW_LEAD_SECS + 1)), boundary + 86_400, "just past it");
        assert_eq!(first_turnable(time, None), boundary + 8 * 86_400, "none in effect or waiting");
    }
}
