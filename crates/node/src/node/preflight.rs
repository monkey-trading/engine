//! Persisted recovery starts only after the authenticated chain anchor passes.

use anyhow::anyhow;
use engine_core::emissions::{Emissions, TokenUnits};
use engine_core::staking::StakingConfig;
use tracing::error;

use crate::checkpoints::emission_budget::check_recorded;
use crate::checkpoints::setup::Setup;
use crate::checkpoints::{CheckpointChain, Poster};
use crate::config::NodeConfig;
use crate::custody::{ContractTerms, StakingTerms, StakingTermsRefused};
use crate::log::Checkpoint;
use crate::node::ready::Ready;
use crate::sequencer::ReplayedLog;

/// Reads the emission period budget the configured minter's MONKEY cap was derived from, and refuses a
/// log that records another: entitlements past the cap could never be claimed. Runs before anything is
/// written.
/// Error cases:
/// - custody is not EVM, so no minter is configured, or the cap cannot be read (alert
///   `checkpoint_startup_failed`)
/// - the log records emissions with another period budget (alert `emission_budget_mismatch`)
pub(super) async fn emission_preflight<R: CheckpointChain>(
    cfg: &NodeConfig,
    poster: &Poster<R>,
    recorded: Option<&Emissions>,
) -> anyhow::Result<TokenUnits> {
    let budget = match &cfg.custody.evm {
        Some(evm) => poster.chain.emission_budget(evm.minter).await,
        None => Err(anyhow!("checkpoint custody is not EVM")),
    }
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
    if recorded.is_none() && genesis.is_none() {
        return Ok(());
    }
    read_terms(Some(terms), recorded, genesis).await.map(drop)
}

/// `staking_preflight`, but always reading the contract's terms when there is a contract, and
/// returning them: a member's start keeps them to check the staking config its tail introduces
/// (`check_held`).
/// Error cases:
/// - see `staking_preflight`
pub(super) async fn read_terms(
    terms: Option<&StakingTerms>,
    recorded: Option<&StakingConfig>,
    genesis: Option<&StakingConfig>,
) -> anyhow::Result<Option<ContractTerms>> {
    let Some(terms) = terms else { return Ok(None) };
    let first = if recorded.is_none() && genesis.is_some() { "the config's genesis" } else { "the log" };
    // One read of the contract, so both configs are compared with the values of one admitted quorum.
    let held = terms.read().await.map_err(|refused| refuse(first, refused))?;
    check_held(held, recorded, genesis)?;
    Ok(Some(held))
}

/// Refuses a log or genesis staking config that differs from `held`, the terms read before, with no
/// read of the chain.
/// Error cases:
/// - the log's or the genesis's epoch or cooldown differs (alert `staking_terms_mismatch`)
pub(super) fn check_held(
    held: ContractTerms,
    recorded: Option<&StakingConfig>,
    genesis: Option<&StakingConfig>,
) -> anyhow::Result<()> {
    for (source, config) in [("the log", recorded), ("the config's genesis", genesis)] {
        let Some(config) = config else { continue };
        held.check(config).map_err(|refused| refuse(source, refused))?;
    }
    Ok(())
}

/// Raises the alert `refused` calls for and names `source` in the error.
fn refuse(source: &str, refused: StakingTermsRefused) -> anyhow::Error {
    match &refused {
        StakingTermsRefused::Differs(_) => {
            error!(alert = "staking_terms_mismatch", "refusing to serve: {source}: {refused}");
        }
        StakingTermsRefused::Unread(_) => {
            error!(alert = "checkpoint_startup_failed", "refusing to serve: {source}: {refused}");
        }
    }
    anyhow::anyhow!("{source}: {refused}")
}

/// The emission period budget and the staking terms the API checks emission schedules and
/// `InitStaking` against, after refusing a `log` or genesis that records others. A member's start read
/// the contract's terms before it connected (`ready`), so it compares the final replica's config with
/// them and reads nothing now: an `InitStaking` in the catch-up or the tail is checked too.
/// Error cases:
/// - the staking contract's reader cannot be built (see `StakingTerms::connect`)
/// - see `emission_preflight` and `staking_preflight`
pub(super) async fn chain_terms(
    cfg: &NodeConfig,
    ready: Option<&Ready>,
    setup: Option<&Setup>,
    log: &ReplayedLog,
) -> anyhow::Result<(Option<TokenUnits>, Option<StakingTerms>)> {
    // The API also refuses an emission schedule with another budget than the minter's.
    let emission_budget = match (ready, setup) {
        (Some(ready), _) => ready.emission_budget,
        (None, Some(setup)) => Some(emission_preflight(cfg, &setup.poster, log.emissions()).await?),
        (None, None) => None,
    };
    // Nor may the log or the genesis carry a staking config the staking contract does not hold: a
    // member's start read the terms before it connected (`ready`), and checks the final log against
    // them without another read.
    let staking_terms = StakingTerms::connect(&cfg.custody)?;
    match ready.and_then(|ready| ready.staking_terms) {
        Some(held) => check_held(held, log.staking_config(), cfg.genesis.staking.as_ref())?,
        None if ready.is_none() => {
            staking_preflight(staking_terms.as_ref(), log.staking_config(), cfg.genesis.staking.as_ref()).await?;
        }
        None => {}
    }
    Ok((emission_budget, staking_terms))
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
