//! A daily turn's handover authorisation. From a turn's boundary the vault names its member's release,
//! in the generation the schedule fixed when it drew the turn, without a transaction; a finalised
//! read of the vault shows it only a finality period later. A turn drawn and finalised before its
//! boundary is already in the finalised schedule, so once trusted time reaches the boundary the turn
//! the schedule names then is the only holding named: the incumbent admits, and the successor
//! accepts, its member from that, neither waits on chain after the boundary, and a release the
//! finalised vault still names but the turn does not is refused. A turn an installation ended names
//! no one (`CheckpointChain::scheduled_turn`), and the vault's own release applies again.

use std::sync::Arc;

use anyhow::{Context, ensure};
use engine_core::types::Millis;
use tracing::{info, warn};

use crate::checkpoints::{CheckpointChain, Observation, ScheduledTurn};
use crate::config::NodeConfig;
use crate::handover::{ensure_named, folded_release};
use crate::tee::sealing::Release;

/// The generation in which the vault names `release` at `at` and trusted time `reached_ms`, a time
/// true time has certainly reached, and the turn that names it when one does: the turn the finalised
/// schedule names at `reached_ms`, when there is one, and otherwise the vault's finalised current
/// release once its generation has folded it.
/// Error cases:
/// - the vault or the schedule cannot be read
/// - the turn names another release, or, with none, the vault does (see `ensure_named`)
pub(crate) async fn named_generation(
    vault: &impl CheckpointChain,
    at: &Observation,
    release: &Release,
    reached_ms: Option<Millis>,
) -> anyhow::Result<(u64, Option<ScheduledTurn>)> {
    if let Some(turn) = turn_at(vault, at, reached_ms).await? {
        ensure_turn_names(&turn, release)?;
        return Ok((turn.generation, Some(turn)));
    }
    Ok((ensure_named(folded_release(vault, at).await?, release)?, None))
}

/// The turn the finalised schedule at `at` names at `reached_ms`, whatever its member; `None` without
/// a time, a schedule, a drawn turn by then, or once an installation ended the turns.
/// Error cases:
/// - the schedule cannot be read
pub(crate) async fn turn_at(
    vault: &impl CheckpointChain,
    at: &Observation,
    reached_ms: Option<Millis>,
) -> anyhow::Result<Option<ScheduledTurn>> {
    let Some(reached) = reached_ms else { return Ok(None) };
    let turn = vault.scheduled_turn(at, reached).await.context("reading the operator schedule")?;
    Ok(turn.filter(|turn| reached >= turn.boundary))
}

/// Refuses `release` unless `turn` names it.
/// Error cases:
/// - the turn's member runs another release
pub(crate) fn ensure_turn_names(turn: &ScheduledTurn, release: &Release) -> anyhow::Result<()> {
    ensure!(
        turn.release == release.id(),
        "the finalised schedule names release 0x{} for turn {}, which trusted time has reached, not 0x{} \
         (launch measurement 0x{}, config 0x{})",
        hex::encode(turn.release),
        turn.turn,
        hex::encode(release.id()),
        hex::encode(release.launch_digest),
        hex::encode(release.config_sha256)
    );
    info!(turn = turn.turn, generation = turn.generation, boundary = turn.boundary, "the turn names this release");
    Ok(())
}

/// The turn the finalised schedule names now, by a one-off Roughtime sample, at a start; `None` when
/// the vault's registry names no schedule or no turn names anyone. A start needs no sample then.
/// Error cases:
/// - the vault cannot be read, or, with a turn drawn, no trusted time sample can be taken
pub(crate) async fn at_start(cfg: &NodeConfig, vault: &impl CheckpointChain) -> anyhow::Result<Option<ScheduledTurn>> {
    let at = vault.observe(crate::checkpoints::View::Finalized, &[]).await?;
    if vault.scheduled_turn(&at, engine_core::types::MAX_STAMP_MS).await?.is_none() {
        return Ok(None);
    }
    let clock: Arc<dyn crate::clock::Clock> = Arc::new(crate::clock::SystemClock);
    let sample = crate::trusted_time::sample_once(&cfg.time, &clock).await.context("taking a trusted time sample")?;
    let reached = sample.earliest_ms.saturating_add(sample.at.elapsed().as_millis() as Millis);
    turn_at(vault, &at, Some(reached)).await
}

/// Whether the turn the finalised schedule names at trusted time `reached_ms` holds `sealed` in
/// `generation`; `None` when no turn names anyone, or the schedule cannot be read, so the vault's own
/// release decides.
pub(crate) async fn names(
    vault: &impl CheckpointChain,
    sealed: &Release,
    generation: u64,
    reached_ms: Option<Millis>,
) -> Option<bool> {
    let turn = async {
        let at = vault.observe(crate::checkpoints::View::Finalized, &[]).await?;
        turn_at(vault, &at, reached_ms).await
    };
    match turn.await {
        Ok(turn) => turn.map(|turn| turn.release == sealed.id() && turn.generation == generation),
        Err(e) => {
            warn!("reading the operator schedule failed: {e:#}");
            None
        }
    }
}
