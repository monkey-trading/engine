//! Persisted recovery starts only after the authenticated chain anchor passes.

use engine_core::emissions::{Emissions, TokenUnits};
use tracing::error;

use crate::checkpoints::emission_budget::check_recorded;
use crate::checkpoints::{CheckpointChain, Poster};
use crate::history::{History, HistoryPlan};
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

/// Authorizes opening or repairing history only after custody rollback and signer checks.
/// Error cases:
/// - anchor, signer, or authorized history recovery fails
pub(super) async fn finish_preflight<R: CheckpointChain>(
    plan: HistoryPlan,
    checkpoint: Option<(&Poster<R>, Option<&Checkpoint>)>,
    recovery: Option<(&crate::sequencer::ReplayedLog, &crate::sequencer::SequencerConfig<'_>)>,
) -> anyhow::Result<History> {
    if let Some((log, cfg)) = recovery {
        log.validate_recovery(cfg)
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
    if let Some((poster, anchor)) = checkpoint {
        poster
            .startup(anchor)
            .await
            .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))?;
    }
    plan.finish().inspect_err(|e| error!(alert = "history_recovery_failed", "refusing to serve: {e:#}"))
}

#[cfg(test)]
mod emission_tests;
#[cfg(test)]
mod tests;
