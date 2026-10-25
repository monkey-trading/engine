//! Checkpoint verification shared by replay, recovery, and custody.

use anyhow::{Context, ensure};
use engine_core::Engine;
use engine_core::auth::ClientAuth;
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;

use crate::log::{Checkpoint, CheckpointDomain};
use crate::tee::{checkpoint_digest, checkpoint_digest_v2};

/// Verifies the root signature and its deployment against the log's protocol domain.
/// Error cases:
/// - a historical checkpoint has no domain recorded anywhere
/// - a domain, root, signer, or signature is malformed or inconsistent
#[cfg(test)]
pub(crate) fn verify(cp: &Checkpoint, engine: &Engine) -> anyhow::Result<CheckpointDomain> {
    verify_with_domain(cp, engine, None)
}

/// Verifies with an explicit deployment for logs that predate recording their domain.
/// Error cases:
/// - the explicit, logged, or checkpoint domain conflicts, or signature verification fails
pub(crate) fn verify_with_domain(
    cp: &Checkpoint,
    engine: &Engine,
    explicit: Option<CheckpointDomain>,
) -> anyhow::Result<CheckpointDomain> {
    let recorded = engine
        .operator()
        .map(|operator| CheckpointDomain { chain_id: operator.chain_id, vault: operator.vault })
        .or_else(|| match engine.client_auth() {
            Some(ClientAuth::Eip712 { chain_id, vault }) => {
                Some(CheckpointDomain { chain_id: *chain_id, vault: *vault })
            }
            _ => None,
        });
    if let (Some(domain), Some(recorded)) = (cp.domain, recorded) {
        ensure!(domain == recorded, "checkpoint domain differs from the log at seq {}", cp.seq);
    }
    if let Some(explicit) = explicit {
        ensure!(
            recorded.is_none_or(|recorded| recorded == explicit) && cp.domain.is_none_or(|domain| domain == explicit),
            "checkpoint domain differs from the supplied deployment"
        );
    }
    let domain = cp
        .domain
        .or(recorded)
        .or(explicit)
        .context("checkpoint has no recorded chain id and vault; supply --chain-id and --vault for an early log")?;
    verify_domain(cp, domain)?;
    Ok(domain)
}

/// Verifies a checkpoint in the custody domain the caller trusts.
/// Error cases:
/// - a root, signer, signature, or domain is malformed or inconsistent
pub(crate) fn verify_domain(cp: &Checkpoint, domain: CheckpointDomain) -> anyhow::Result<()> {
    ensure!(cp.domain.is_none_or(|value| value == domain), "checkpoint domain mismatch");
    let root: B256 = cp.root.parse().context("checkpoint root")?;
    let signer: AccountId = cp.signer.parse().context("checkpoint signer")?;
    let signature: Signature = cp.signature.parse().context("checkpoint signature is missing or malformed")?;
    let digest = if let Some(parent) = cp.parent {
        ensure!(
            if parent.has_checkpoint {
                parent.seq < cp.seq
            } else {
                parent.seq == 0 && parent.root == B256::from([0; 32])
            },
            "invalid checkpoint parent"
        );
        checkpoint_digest_v2(domain.chain_id, &domain.vault.0, cp.seq, &root.0, parent)
    } else {
        checkpoint_digest(domain.chain_id, &domain.vault.0, cp.seq, &root.0)
    };
    ensure!(
        signer.0 != [0; 20] && eip712::recover(&B256::from(digest), &signature) == Some(signer),
        "invalid checkpoint signature at seq {}",
        cp.seq
    );
    Ok(())
}

/// Authenticates completion evidence against independently supplied authority and deployment.
/// Error cases:
/// - no trusted authority/domain is supplied, domain differs, or the signature is invalid
pub(crate) fn verify_withdrawal(
    w: &crate::sequencer::SignedWithdrawal,
    domain: Option<CheckpointDomain>,
    trusted: Option<AccountId>,
) -> anyhow::Result<()> {
    let trusted = trusted.context("withdrawal completion requires a trusted signer; supply --signer")?;
    let domain = domain.context("withdrawal completion requires a trusted deployment")?;
    if w.authorization.is_some() {
        return crate::withdrawals::verify(w, domain, trusted);
    }
    ensure!(w.chain_id == domain.chain_id && w.vault == domain.vault, "withdrawal completion domain mismatch");
    let signature: Signature =
        w.signature.parse().context("withdrawal completion signature is missing or malformed")?;
    let digest = crate::tee::withdrawal_digest(w.chain_id, &w.vault.0, w.account, w.amount, w.nonce);
    ensure!(
        trusted.0 != [0; 20] && eip712::recover(&B256::from(digest), &signature) == Some(trusted),
        "unauthorized withdrawal completion signer"
    );
    Ok(())
}
