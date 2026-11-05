//! A daily turn's handover authorisation. From a turn's boundary the vault names its member's release,
//! in the generation the schedule fixed when it drew the turn, without a transaction; a finalised
//! read of the vault shows it only a finality period later. The turn was drawn a day ahead, so the
//! finalised schedule already names it: the incumbent admits, and the successor accepts, the turn's
//! member from that once trusted time reaches the boundary, and neither waits on chain after it.

use anyhow::Context;
use engine_core::types::Millis;
use tracing::info;

use crate::checkpoints::{CheckpointChain, Observation, ScheduledTurn};
use crate::handover::{ensure_named, folded_release};
use crate::tee::sealing::Release;

/// The generation in which the vault names `release` at `at`, or from trusted time `reached_ms`: the
/// vault's finalised current release once its generation has folded it, or else the turn the
/// finalised schedule names at `reached_ms`, a time true time has certainly reached, when its member
/// runs `release` and the turn's boundary has passed.
/// Error cases:
/// - the vault cannot be read, or names another release both ways (see `ensure_named`)
pub(crate) async fn named_generation(
    vault: &impl CheckpointChain,
    at: &Observation,
    release: &Release,
    reached_ms: Option<Millis>,
) -> anyhow::Result<u64> {
    let current = folded_release(vault, at).await;
    if let Ok(current) = &current
        && current.id == release.id()
    {
        return Ok(current.generation);
    }
    if let Some(turn) = scheduled_for(vault, at, release, reached_ms).await? {
        return Ok(turn.generation);
    }
    ensure_named(current?, release)
}

/// The turn the finalised schedule at `at` names for `release` at `reached_ms`, once its boundary
/// has passed; `None` without a time, a schedule, or such a turn.
/// Error cases:
/// - the schedule cannot be read
pub(crate) async fn scheduled_for(
    vault: &impl CheckpointChain,
    at: &Observation,
    release: &Release,
    reached_ms: Option<Millis>,
) -> anyhow::Result<Option<ScheduledTurn>> {
    let Some(reached) = reached_ms else { return Ok(None) };
    let turn = vault.scheduled_turn(at, reached).await.context("reading the operator schedule")?;
    let turn = turn.filter(|turn| turn.release == release.id() && reached >= turn.boundary);
    if let Some(turn) = &turn {
        info!(
            turn = turn.turn,
            generation = turn.generation,
            boundary = turn.boundary,
            "the finalised schedule names this release for the turn trusted time has reached"
        );
    }
    Ok(turn)
}
