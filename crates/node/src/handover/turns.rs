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

use crate::checkpoints::{CheckpointChain, HandoverSource, Observation, ScheduledTurn};
use crate::config::NodeConfig;
use crate::handover::{ensure_named, folded_release};
use crate::tee::Attestor;
use crate::tee::sealing::{KeyStore, Release, Sealer};

/// How many drawn turns back from its own a member that finds no source holding the keys looks: a
/// week of members that never took their turns, past which the governance fallback applies.
const SOURCE_LOOKBACK: usize = 7;

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

/// The turn the finalised schedule names now, at a start, by `reached`, the time trusted time has
/// reached, awaited only once a turn is drawn (`drawn`); `None` when none is, or none names anyone
/// by then. A start needs no sample without one.
/// Error cases:
/// - the vault cannot be read, or, with a turn drawn, `reached` fails
pub(crate) async fn at_start(
    vault: &impl CheckpointChain,
    reached: impl Future<Output = anyhow::Result<Millis>>,
) -> anyhow::Result<Option<ScheduledTurn>> {
    let at = vault.observe(crate::checkpoints::View::Finalized, &[]).await?;
    if !drawn(vault, &at).await? {
        return Ok(None);
    }
    turn_at(vault, &at, Some(reached.await?)).await
}

/// The time true time has certainly reached, by a one-off Roughtime sample of `cfg`'s servers.
/// Error cases:
/// - no trusted time sample can be taken
pub(crate) async fn sampled_now(cfg: &NodeConfig) -> anyhow::Result<Millis> {
    let clock: Arc<dyn crate::clock::Clock> = Arc::new(crate::clock::SystemClock);
    let sample = crate::trusted_time::sample_once(&cfg.time, &clock).await.context("taking a trusted time sample")?;
    Ok(sample.earliest_ms.saturating_add(sample.at.elapsed().as_millis() as Millis))
}

/// Whether the finalised schedule at `at` has drawn a turn that names its member at some time, even
/// one the finalised block has yet to reach: false when the vault's registry names no schedule, no
/// turn is drawn, or an installation ended the turns.
/// Error cases:
/// - the schedule cannot be read
pub(crate) async fn drawn(vault: &impl CheckpointChain, at: &Observation) -> anyhow::Result<bool> {
    Ok(vault.scheduled_turn(at, engine_core::types::MAX_STAMP_MS).await?.is_some())
}

/// Whether the turn the finalised schedule names at trusted time `reached_ms` holds `sealed` in
/// `generation`, and that turn's boundary; `None` when no turn names anyone, or the schedule cannot
/// be read, so the vault's own release decides.
pub(crate) async fn names(
    vault: &impl CheckpointChain,
    sealed: &Release,
    generation: u64,
    reached_ms: Option<Millis>,
) -> Option<(bool, Millis)> {
    let turn = async {
        let at = vault.observe(crate::checkpoints::View::Finalized, &[]).await?;
        turn_at(vault, &at, reached_ms).await
    };
    match turn.await {
        Ok(turn) => turn.map(|turn| (turn.release == sealed.id() && turn.generation == generation, turn.boundary)),
        Err(e) => {
            warn!("reading the operator schedule failed: {e:#}");
            None
        }
    }
}

/// The operators a member of `turn` may take the keys from, in the order it tries them: the turn's
/// source, the operator the registry named when the turn was drawn, then each earlier drawn turn's
/// source, up to `SOURCE_LOOKBACK` turns back, since a member that never took its turn leaves the
/// keys with the one before it. Each distinct operator with a handover endpoint, never the member.
/// Error cases:
/// - the schedule cannot be read
pub(crate) async fn sources(
    vault: &impl CheckpointChain,
    at: &Observation,
    turn: &ScheduledTurn,
) -> anyhow::Result<Vec<HandoverSource>> {
    let mut sources: Vec<HandoverSource> = Vec::new();
    let mut drawn = Some(turn.clone());
    for _ in 0..=SOURCE_LOOKBACK {
        let Some(this) = drawn.take() else { break };
        if let Some(source) = this.source.as_ref().filter(|source| {
            source.candidate != turn.candidate
                && !source.handover_endpoint.is_empty()
                && sources.iter().all(|listed| listed.candidate != source.candidate)
        }) {
            sources.push(source.clone());
        }
        drawn = vault.scheduled_turn(at, this.boundary - 1).await.context("reading the operator schedule")?;
    }
    Ok(sources)
}

/// Takes `turn`'s keys, which the finalised schedule at `at` names this release for, from the first
/// of its `sources` that hands them over: each one's published log first, then the keys and the log's
/// tail by the handover, with trusted time at the turn's boundary.
/// Error cases:
/// - no source has a handover endpoint, the vault's signers cannot be read, or every source fails
pub(crate) async fn take_keys(
    cfg: &NodeConfig,
    vault: &impl CheckpointChain,
    at: &Observation,
    turn: &ScheduledTurn,
    sealer: &dyn Sealer,
    attestor: &dyn Attestor,
    metrics: &crate::metrics::Metrics,
) -> anyhow::Result<KeyStore> {
    let sources = sources(vault, at, turn).await?;
    ensure!(
        !sources.is_empty(),
        "turn {} names this release but no operator with a handover endpoint to take its keys from",
        turn.turn
    );
    let signers = crate::handover::vault_signers(vault, at).await?;
    let mut failures = Vec::new();
    for source in sources {
        info!(turn = turn.turn, predecessor = %source.handover_endpoint, "daily turn: taking the keys");
        let source = crate::handover::Source {
            endpoint: source.handover_endpoint,
            api_hostname: source.api_hostname,
            release: source.release,
            candidate: source.candidate,
        };
        let taken = async {
            crate::handover::sync_predecessor_log(cfg, &source, signers.clone(), metrics).await?;
            crate::handover::take(cfg, &source.endpoint, sealer, attestor, vault, Some(turn.boundary), metrics).await
        };
        match taken.await {
            Ok(store) => return Ok(store),
            Err(e) => {
                warn!(predecessor = %source.endpoint, "daily turn: this operator handed no keys over: {e:#}");
                failures.push(format!("{}: {e:#}", source.endpoint));
            }
        }
    }
    anyhow::bail!("no operator handed turn {}'s keys over: {}", turn.turn, failures.join("; "))
}

#[cfg(test)]
mod tests;
