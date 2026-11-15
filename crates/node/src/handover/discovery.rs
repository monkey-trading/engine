//! Where a turn's keys are (#596). The handover endpoint the finalised schedule records for a turn's
//! source names who held the keys when the turn was drawn, not who holds them now: a member that took
//! the turn late and was then skipped holds them, and a skip, or a reorg that removed one, moves who
//! should. So a member taking a turn, or following its holder, asks first the operator whose release
//! signed the signer epoch's newest head, which is the enclave holding the keys whatever assignment
//! it binds; then each other member of the turn's set that held an assignment of it; then the turn's
//! source and the holders of earlier turns (`turns::sources`). A head's release is a first guess and
//! never proof: each operator is asked in turn, and only the one that hands the keys over holds them.

use alloy::primitives::Address;
use anyhow::Context;
use engine_core::types::AccountId;
use futures_util::{StreamExt, TryStreamExt, stream};

use crate::checkpoints::{CheckpointChain, HandoverSource, Observation, ScheduledTurn};
use crate::handover::assigned::{Assigned, call};
use crate::liveness::chain::setOfCall;
use crate::standby_key::chain::{AgreedBlock, StandbyDomain, candidateOfCall, heldTurnCall};

/// How many of a set's members are read at once.
const MEMBER_READS: usize = 8;

/// A member of the turn's set as the latest agreed block registers it, and whether it held an
/// assignment of the turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Registered {
    pub source: HandoverSource,
    pub held: bool,
}

/// The operators a member taking `turn` asks for the keys, in the order the module states, with
/// `members` the turn's set as `members` reads it at the latest agreed block, `assigned` the turn's
/// current assignment there, and the turn's source and earlier holders read from the finalised
/// schedule at `at`. Each distinct operator with a handover endpoint, never `turn`'s member.
/// Error cases:
/// - the finalised schedule cannot be read
pub(crate) async fn sources(
    vault: &impl CheckpointChain,
    at: &Observation,
    turn: &ScheduledTurn,
    assigned: &Assigned,
    members: &[Registered],
) -> anyhow::Result<Vec<HandoverSource>> {
    let earlier = crate::handover::turns::sources(vault, at, turn).await?;
    Ok(ordered(turn.candidate, assigned.head_release, members, earlier))
}

/// The operator whose release signed the signer epoch's newest head, among `members` and `earlier`,
/// the turn's source and earlier holders: the enclave holding the keys; `None` with no head, or when
/// no operator listed runs that release.
pub(crate) fn holder<'a>(
    head_release: Option<[u8; 32]>,
    members: &'a [Registered],
    earlier: &'a [HandoverSource],
) -> Option<&'a HandoverSource> {
    let release = head_release?;
    members.iter().map(|member| &member.source).chain(earlier).find(|source| source.release == release)
}

/// `sources`' order, for a turn whose member is `member`: the head's holder, the set's other members
/// that held an assignment of the turn, then `earlier`, each once and with an endpoint.
pub(crate) fn ordered(
    member: AccountId,
    head_release: Option<[u8; 32]>,
    members: &[Registered],
    earlier: Vec<HandoverSource>,
) -> Vec<HandoverSource> {
    let first = holder(head_release, members, &earlier).cloned();
    let held = members.iter().filter(|registered| registered.held).map(|registered| registered.source.clone());
    let mut sources: Vec<HandoverSource> = Vec::new();
    for source in first.into_iter().chain(held).chain(earlier) {
        if source.candidate != member
            && !source.handover_endpoint.is_empty()
            && sources.iter().all(|listed| listed.candidate != source.candidate)
        {
            sources.push(source);
        }
    }
    sources
}

/// The members of set `set` at `block`, each with its registration and whether it held an assignment
/// of `turn`, in the set's order.
/// Error cases:
/// - a call fails or does not decode, or the vault names no schedule
pub(crate) async fn members(
    block: &impl AgreedBlock,
    chain_id: u64,
    vault: Address,
    (set, turn): (u32, u64),
) -> anyhow::Result<Vec<Registered>> {
    let domain =
        StandbyDomain::at_if_scheduled(block, chain_id, vault).await?.context("the registry names no schedule")?;
    let listed = call(block, "discovery_set", domain.schedule, setOfCall { setId: set }).await?;
    let member = |candidate: Address| async move {
        let registered = call(block, "discovery_candidate", domain.registry, candidateOfCall { candidate }).await?;
        let held = call(block, "discovery_held", domain.schedule, heldTurnCall { turn, candidate }).await?;
        anyhow::Ok(Registered {
            source: HandoverSource {
                candidate: AccountId(candidate.0.0),
                release: registered.release.0,
                api_hostname: registered.apiHostname,
                handover_endpoint: registered.handoverEndpoint,
            },
            held,
        })
    };
    stream::iter(listed.candidates).map(member).buffered(MEMBER_READS).try_collect().await
}

#[cfg(test)]
mod tests;
