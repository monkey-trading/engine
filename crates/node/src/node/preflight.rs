//! Persisted recovery starts only after the authenticated chain anchor passes.

use engine_core::emissions::{Emissions, TokenUnits};
use engine_core::staking::StakingConfig;
use tracing::error;

use crate::checkpoints::emission_budget::check_recorded;
use crate::checkpoints::{CheckpointChain, Poster};
use crate::custody::{StakingTerms, StakingTermsRefused};
use crate::log::Checkpoint;

/// Reads the emission period budget the vault's MONKEY cap was derived from, and refuses a log that
/// records another: entitlements past the cap could never be claimed. Runs before anything is
/// written.
/// Error cases:
/// - the cap cannot be read (alert `checkpoint_startup_failed`)
/// - the log records emissions with another period budget (alert `emission_budget_mismatch`)
pub(super) async fn emission_preflight<R: CheckpointChain>(
    poster: &Poster<R>,
    recorded: Option<&Emissions>,
) -> anyhow::Result<TokenUnits> {
    let budget = poster
        .chain
        .emission_budget()
        .await
        .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))?;
    check_recorded(budget, recorded)
        .inspect_err(|e| error!(alert = "emission_budget_mismatch", "refusing to serve: {e:#}"))?;
    Ok(budget)
}

/// Refuses a log that records, or a config whose genesis would record, a staking config with another
/// epoch or cooldown than the configured staking contract holds: the engine's epochs would close on a
/// schedule the contract's cooldown does not cover, and neither side can change. Nothing is read
/// without a staking contract to compare with or a config to compare. Runs before anything is
/// written.
/// Error cases:
/// - the contract cannot be read (alert `checkpoint_startup_failed`)
/// - the log's or the genesis's epoch or cooldown differs from the contract's (alert
///   `staking_terms_mismatch`)
pub(super) async fn staking_preflight(
    terms: Option<&StakingTerms>,
    recorded: Option<&StakingConfig>,
    genesis: Option<&StakingConfig>,
) -> anyhow::Result<()> {
    let Some(terms) = terms else { return Ok(()) };
    let sources = [("the log", recorded), ("the config's genesis", genesis)];
    let Some((first, _)) = sources.iter().find(|(_, config)| config.is_some()) else { return Ok(()) };
    let refuse = |source: &str, refused: StakingTermsRefused| {
        match &refused {
            StakingTermsRefused::Differs(_) => {
                error!(alert = "staking_terms_mismatch", "refusing to serve: {source}: {refused}");
            }
            StakingTermsRefused::Unread(_) => {
                error!(alert = "checkpoint_startup_failed", "refusing to serve: {source}: {refused}");
            }
        }
        anyhow::anyhow!("{source}: {refused}")
    };
    // One read of the contract, so both configs are compared with the values of one admitted quorum.
    let held = terms.read().await.map_err(|refused| refuse(first, refused))?;
    for (source, config) in sources {
        let Some(config) = config else { continue };
        held.check(config).map_err(|refused| refuse(source, refused))?;
    }
    Ok(())
}

/// Authorizes opening or repairing history only after custody rollback and signer checks.
/// Error cases:
/// - anchor, signer, or authorized history recovery fails
#[cfg(test)]
pub(super) async fn finish_preflight<R: CheckpointChain>(
    plan: crate::history::HistoryPlan,
    checkpoint: Option<(&Poster<R>, Option<&Checkpoint>)>,
    recovery: Option<(
        &crate::sequencer::ReplayedLog,
        &crate::sequencer::SequencerConfig<'_>,
        engine_core::types::AccountId,
    )>,
) -> anyhow::Result<crate::history::History> {
    authorize(checkpoint, recovery, false).await?;
    plan.finish().inspect_err(|e| error!(alert = "history_recovery_failed", "refusing to serve: {e:#}"))
}

/// Finishes all independent recovery checks before any history, catalog or append mutation. With
/// `anchored`, the vault already authenticated the anchor, before a member connected for its tail
/// (`ready`), and nothing here reads the chain.
/// Error cases:
/// - pure recovery, reserved-work or independently finalized anchor validation fails
pub(super) async fn authorize<R: CheckpointChain>(
    checkpoint: Option<(&Poster<R>, Option<&Checkpoint>)>,
    recovery: Option<(
        &crate::sequencer::ReplayedLog,
        &crate::sequencer::SequencerConfig<'_>,
        engine_core::types::AccountId,
    )>,
    anchored: bool,
) -> anyhow::Result<()> {
    if let Some((log, cfg, signer)) = recovery {
        log.validate_recovery(cfg, signer)
            .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
        if let Some((poster, _)) = checkpoint {
            let reserved = poster
                .chain
                .reserved_checkpoint()
                .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))?;
            log.validate_submission(cfg.checkpoint_parent, reserved.as_ref())
                .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))?;
        }
    }
    if let Some((poster, anchor)) = checkpoint.filter(|_| !anchored) {
        poster
            .startup(anchor)
            .await
            .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod emission_tests;
#[cfg(test)]
mod staking_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod unscanned;
