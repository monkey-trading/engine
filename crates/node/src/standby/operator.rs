//! The operator a follower follows. Under rotation it is the candidate the registry names in effect,
//! at the API hostname its candidacy registered (the read the ACME hostname check uses), and every
//! frame verifies against the vault's signer at finality: neither the host nor the config names it.
//! Without rotation it is the one `[standby]` names.

use std::collections::BTreeSet;

use anyhow::{Context, ensure};
use engine_core::types::AccountId;

use crate::checkpoints::{CheckpointChain, View};
use crate::config::{NodeConfig, StandbySection};

/// Where a follower reads the operator's stream and archive, and the enclave its frames verify against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Followed {
    /// The operator's API origin, for its `/v1/stream` and its archive.
    pub origin: String,
    /// The enclave address every frame and the copy's commands verify against.
    pub enclave: AccountId,
    /// Archive mirrors, tried before the origin, which may be down exactly when the archive matters.
    pub mirrors: Vec<String>,
    /// Accept the operator's self-signed or private-CA certificate: development only.
    pub danger_accept_invalid_certs: bool,
}

impl Followed {
    /// The operator `[standby]` names.
    pub(crate) fn from_standby(standby: &StandbySection) -> Self {
        Self {
            origin: standby.operator.clone(),
            enclave: standby.enclave,
            mirrors: standby.mirrors.clone(),
            danger_accept_invalid_certs: standby.danger_accept_invalid_certs,
        }
    }

    /// Whether this operator is another than `other` to follow: another origin, or another enclave at
    /// the same one, as a `setSigner` replacement at an unchanged hostname is.
    pub(crate) fn moved_from(&self, other: &Followed) -> bool {
        self.origin != other.origin || self.enclave != other.enclave
    }

    /// The archive origins in fetch order: the mirrors first, the operator itself last.
    pub(crate) fn origins(&self) -> Vec<String> {
        self.mirrors.iter().cloned().chain(std::iter::once(self.origin.clone())).collect()
    }
}

/// The operator in effect: with `operator.rotation_enabled`, the registry's candidate in effect at
/// `https://` its registered API hostname, with the vault's signer at finality and the archive
/// settings of `[tee.sealing.handover]`; otherwise the one `[standby]` names.
/// Error cases:
/// - rotation is off and there is no `[standby]`
/// - the chain cannot be read, or the registry records no hostname for the operator in effect
pub(crate) async fn resolve(cfg: &NodeConfig, vault: &impl CheckpointChain) -> anyhow::Result<Followed> {
    if !cfg.operator.rotation_enabled {
        let standby =
            cfg.standby.as_ref().context("with rotation off, a follower follows the operator [standby] names")?;
        return Ok(Followed::from_standby(standby));
    }
    let hostname = vault.operator_hostname().await.context("reading the operator in effect's API hostname")?;
    ensure!(!hostname.is_empty(), "the registry records no API hostname for the operator in effect");
    let enclave = vault.state(View::Finalized).await.context("reading the vault's signer at finality")?.signer;
    let handover = cfg.tee.sealing.as_ref().and_then(|sealing| sealing.handover.as_ref());
    Ok(Followed {
        origin: format!("https://{hostname}"),
        enclave,
        mirrors: handover.map(|handover| handover.mirrors.clone()).unwrap_or_default(),
        danger_accept_invalid_certs: handover.is_some_and(|handover| handover.danger_accept_invalid_certs),
    })
}

/// The operator a `--standby` node follows: as `resolve` names it, with `[standby]`'s archive mirrors
/// and certificate setting; with rotation on, the registry's candidate in effect, read again each
/// stint, so it moves with each turn and election, and `[standby]`'s operator only with rotation off.
/// Error cases:
/// - see `resolve`
pub(crate) async fn for_standby(
    cfg: &NodeConfig,
    standby: &StandbySection,
    vault: &impl CheckpointChain,
) -> anyhow::Result<Followed> {
    let resolved = resolve(cfg, vault).await?;
    Ok(Followed {
        mirrors: standby.mirrors.clone(),
        danger_accept_invalid_certs: standby.danger_accept_invalid_certs,
        ..resolved
    })
}

/// The signers a promoted standby's copy must replay against before its takeover truncates it: with
/// rotation on, every signer the vault recorded at finality, since turns and elections move the
/// operator and a signer replacement moves its enclave, so `[standby].enclave` need not have signed
/// the copy; with rotation off, the enclave `[standby]` names.
/// Error cases:
/// - with rotation on, the vault cannot be read
pub(crate) async fn takeover_signers(
    cfg: &NodeConfig,
    standby: &StandbySection,
    vault: &impl CheckpointChain,
) -> anyhow::Result<BTreeSet<AccountId>> {
    if !cfg.operator.rotation_enabled {
        return Ok(BTreeSet::from([standby.enclave]));
    }
    let at = vault.observe(View::Finalized, &[]).await.context("reading the vault's signers at finality")?;
    crate::handover::vault_signers(vault, &at).await
}

#[cfg(test)]
mod tests;
