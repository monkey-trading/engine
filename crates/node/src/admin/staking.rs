//! `engine admin init-staking`'s check of the epoch and cooldown it is about to sign against the
//! staking contract's, which the node also makes before it sequences them (`custody::StakingTerms`).
//! A mistyped value stops here, before anything is signed or sent.

use std::path::Path;

use anyhow::{Context, anyhow};
use engine_core::staking::StakingConfig;
use tracing::warn;

use crate::config::NodeConfig;
use crate::custody::StakingTerms;
use crate::secrets::Bundle;

/// Reads the configured staking contract's epoch and cooldown through the config's custody
/// providers, whose delivered keys come from `secrets`, and compares them with `staking`'s. A config
/// with no staking contract, as under mock custody, has nothing to compare with.
/// Error cases:
/// - `secrets` is not a bundle, or the providers name delivered keys and it holds none for them
/// - the contract cannot be read through a quorum of providers
/// - the epoch or the cooldown is not the contract's
pub(super) async fn check(mut cfg: NodeConfig, secrets: Option<&Path>, staking: &StakingConfig) -> anyhow::Result<()> {
    if let Some(file) = secrets {
        cfg.deliver_urls(&Bundle::from_file(file)?.secrets)?;
    }
    let hint = if secrets.is_some() { "" } else { "; providers that name delivered keys need --secrets-file" };
    let Some(terms) =
        StakingTerms::connect(&cfg.custody).with_context(|| format!("connecting to the custody providers{hint}"))?
    else {
        warn!("the config names no staking contract: the epoch and cooldown are not checked against one");
        return Ok(());
    };
    terms.check(staking).await.map_err(|refused| anyhow!("refusing to sign: {refused}"))
}

#[cfg(test)]
mod tests;
