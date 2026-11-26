//! What the holder of the enclave's keys confirms through the custody quorum before it hands the keys,
//! or its delivered secrets, on (`Predecessor`). Its checkpoint endpoint is a single provider, so where
//! an admission rests on what that endpoint reads alone — the vault's current release, its generation
//! and fold, the turn the finalised schedule names, or the registry's rotation switch — a strict
//! majority of the custody providers must read the same at the same block, which they hold final
//! (`ReleaseQuorum`), and any difference refuses the request until they agree, under Warning
//! `checkpoint_endpoint_disagrees`. Where the latest agreed block's assignment already names the release
//! (`predecessor::assignment`), the endpoint's reads can only refuse.

use alloy::primitives::Address;
use alloy::sol;
use anyhow::{Context, ensure};
use engine_core::turns::TURN_MS;
use engine_core::types::{AccountId, Millis};
use futures_util::future::BoxFuture;
use tracing::{info, warn};

use crate::checkpoints::{CheckpointChain, CurrentRelease, Observation, ScheduledTurn};
use crate::handover::assigned::call;
use crate::handover::turns::named_generation;
use crate::liveness::chain::rotationStateCall;
use crate::standby_key::chain::{AgreedBlock, candidateOfCall, registryCall, scheduleCall, turnAtCall};
use crate::tee::sealing::Release;

sol! {
    function currentRelease() external view returns (bytes32);
    function releaseGeneration() external view returns (uint64);
    function syncedRelease() external view returns (bytes32);
    function rotationEnabled() external view returns (bool);
}

/// A turn the finalised schedule names, as an admission decides by it: its member, the admin key and
/// release the member's candidacy registers, and the generation and set the draw fixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnFacts {
    pub turn: u64,
    pub boundary: Millis,
    pub candidate: AccountId,
    pub admin_key: AccountId,
    pub release: [u8; 32],
    pub generation: u64,
    pub set: u32,
}

/// What the custody quorum reads at a block it holds final.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Held {
    /// The vault's current release and its generation.
    pub current: CurrentRelease,
    /// The release the vault's generation has folded.
    pub synced: [u8; 32],
    /// The turn the finalised schedule names at the time asked, whatever its boundary.
    pub turn: Option<TurnFacts>,
}

/// Where the holder reads what its checkpoint endpoint reads, through the custody quorum.
pub(crate) trait ReleaseQuorum: Send + Sync {
    /// What the custody quorum reads at `at`'s block, the checkpoint endpoint's finalized observation,
    /// once a strict majority names that block and holds it final: the vault's current release, its
    /// generation and fold, and, at `now_ms` when given, the turn the finalised schedule names.
    /// Error cases:
    /// - `at` pins no block, or the providers have no quorum, name another block at its height, or do
    ///   not hold it final in time
    fn held<'a>(&'a self, at: &'a Observation, now_ms: Option<Millis>) -> BoxFuture<'a, anyhow::Result<Held>>;

    /// The registry's rotation switch as the custody quorum reads it at `at`'s block, held final as
    /// `held` holds it, and at the providers' agreed head.
    /// Error cases:
    /// - as `held`, or the providers have no quorum at their head
    fn rotation<'a>(&'a self, at: &'a Observation) -> BoxFuture<'a, anyhow::Result<(bool, bool)>>;
}

impl From<&ScheduledTurn> for TurnFacts {
    fn from(turn: &ScheduledTurn) -> Self {
        Self {
            turn: turn.turn,
            boundary: turn.boundary,
            candidate: turn.candidate,
            admin_key: turn.admin_key,
            release: turn.release,
            generation: turn.generation,
            set: turn.set,
        }
    }
}

/// `named_generation` through `vault`, the checkpoint endpoint, at `at`, once `quorum` reads the same
/// at that block: the same turn the finalised schedule names at trusted time `reached_ms`, or, with
/// none, `release` as the vault's current release in the same generation, and its fold where `vault`
/// reads one. A difference pages `checkpoint_endpoint_disagrees`, which an agreement resolves.
/// Error cases:
/// - as `named_generation`
/// - the quorum cannot read the block (`ReleaseQuorum::held`), or reads another turn, release,
///   generation or fold there
pub(crate) async fn confirmed_generation(
    vault: &impl CheckpointChain,
    quorum: &dyn ReleaseQuorum,
    at: &Observation,
    release: &Release,
    reached_ms: Option<Millis>,
) -> anyhow::Result<(u64, Option<ScheduledTurn>)> {
    let named = named_generation(vault, at, release, reached_ms).await?;
    let held = quorum.held(at, reached_ms).await.context("reading the vault's release through the custody quorum")?;
    let agreed = agreement((vault.reads_release_fold(), release, reached_ms), &named, &held);
    match &agreed {
        Ok(()) => info!(
            alert_resolved = "checkpoint_endpoint_disagrees",
            "the custody quorum reads the vault's release as the checkpoint endpoint does"
        ),
        Err(e) => page_disagreement(format_args!("{e:#}")),
    }
    agreed.map(|()| named)
}

/// Pages the checkpoint endpoint reading `what` otherwise than the custody quorum at the same block: a
/// faulty or lying endpoint, which the holder does not act on.
pub(crate) fn page_disagreement(what: std::fmt::Arguments<'_>) {
    warn!(
        alert = "checkpoint_endpoint_disagrees",
        "{what}: this release hands neither its keys nor its secrets over while they disagree, and sequences on"
    );
}

/// Whether `held`, what the custody quorum reads, is what the checkpoint endpoint's `named` rests on: the
/// same turn by trusted time `reached_ms`, or, with none, `release` as the current release in the same
/// generation, folded where the endpoint reads a fold (`reads_fold`).
/// Error cases:
/// - the quorum reads another turn, release, generation or fold
fn agreement(
    (reads_fold, release, reached_ms): (bool, &Release, Option<Millis>),
    named: &(u64, Option<ScheduledTurn>),
    held: &Held,
) -> anyhow::Result<()> {
    let endpoint = named.1.as_ref().map(TurnFacts::from);
    let majority = held.turn.filter(|turn| reached_ms.is_some_and(|reached| reached >= turn.boundary));
    ensure!(
        endpoint == majority,
        "the checkpoint endpoint reads the finalised schedule naming {endpoint:?}, but the custody quorum reads \
         {majority:?} at the same block: the keys stay here until they agree"
    );
    if named.1.is_none() {
        let current = CurrentRelease { id: release.id(), generation: named.0 };
        ensure!(
            held.current == current,
            "the checkpoint endpoint reads release 0x{} in generation {} as the vault's current one, but the custody \
             quorum reads 0x{} in generation {} at the same block: the keys stay here until they agree",
            hex::encode(current.id),
            current.generation,
            hex::encode(held.current.id),
            held.current.generation
        );
        ensure!(
            !reads_fold || held.synced == held.current.id,
            "the checkpoint endpoint reads the vault's generation folding its current release 0x{}, but the custody \
             quorum reads it folding 0x{} at the same block",
            hex::encode(held.current.id),
            hex::encode(held.synced)
        );
    }
    Ok(())
}

/// What `ReleaseQuorum::held` reads at `block` for the vault `vault`.
/// Error cases:
/// - a call fails or does not decode, or the turn's boundary is out of range
pub(crate) async fn held_at(block: &impl AgreedBlock, vault: Address, now_ms: Option<Millis>) -> anyhow::Result<Held> {
    let (current, generation, synced, registry) = tokio::try_join!(
        call(block, "handover_current_release", vault, currentReleaseCall {}),
        call(block, "handover_release_generation", vault, releaseGenerationCall {}),
        call(block, "handover_synced_release", vault, syncedReleaseCall {}),
        call(block, "handover_registry", vault, registryCall {}),
    )?;
    let turn = match now_ms {
        Some(now_ms) if !registry.is_zero() => turn_at(block, registry, now_ms).await?,
        _ => None,
    };
    Ok(Held { current: CurrentRelease { id: current.0, generation }, synced: synced.0, turn })
}

/// The registry's rotation switch at `block` for the vault `vault`, as the checkpoint endpoint reads
/// it: off while the vault names no registry.
/// Error cases:
/// - a call fails or does not decode
pub(crate) async fn rotation_at(block: &impl AgreedBlock, vault: Address) -> anyhow::Result<bool> {
    let registry = call(block, "handover_registry", vault, registryCall {}).await?;
    if registry.is_zero() {
        return Ok(false);
    }
    call(block, "handover_rotation", registry, rotationEnabledCall {}).await
}

/// The turn the schedule of `registry` names at `now_ms`, at `block`, as `CheckpointChain::scheduled_turn`
/// reads it: none without a schedule or a drawn turn by then, or once an installation ended the turns.
/// Error cases:
/// - a call fails or does not decode, `now_ms` is negative, or the turn's boundary is out of range
async fn turn_at(block: &impl AgreedBlock, registry: Address, now_ms: Millis) -> anyhow::Result<Option<TurnFacts>> {
    let schedule = call(block, "handover_schedule", registry, scheduleCall {}).await?;
    if schedule.is_zero() {
        return Ok(None);
    }
    let timestamp = u64::try_from(now_ms / 1000).context("a time before the epoch")?;
    let found = call(block, "handover_turn", schedule, turnAtCall { timestamp }).await?;
    let (turn, drawn) = (found.turn, found._1);
    if drawn.candidate.is_zero() {
        return Ok(None);
    }
    let boundary = engine_core::turns::boundary(turn).context("the turn's boundary is out of range")?;
    let state = call(block, "handover_rotation_state", registry, rotationStateCall {}).await?;
    let ends = u64::try_from(boundary + TURN_MS).ok().map(|ms| ms / 1000);
    if Some(state.until) == ends && state.generation == 0 {
        return Ok(None);
    }
    let member = call(block, "handover_candidate", registry, candidateOfCall { candidate: drawn.candidate }).await?;
    Ok(Some(TurnFacts {
        turn,
        boundary,
        candidate: AccountId(drawn.candidate.0.0),
        admin_key: AccountId(member.adminKey.0.0),
        release: member.release.0,
        generation: drawn.generation,
        set: drawn.setId,
    }))
}

#[cfg(test)]
mod tests;
