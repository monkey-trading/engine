//! Read-only public-log audits using the vault's canonical finalized signer history.

use std::fs;
use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::types::AccountId;

use crate::checkpoints::{APPROVAL_PAGE_SIZE, AcceptedCheckpoint, CheckpointChain, CheckpointRpc, View, signature};
use crate::config::{CustodyProvider, NodeConfig};
use crate::log::{self, Checkpoint, CheckpointDomain, CheckpointParent, LogEntry, SegmentedLog};
use crate::sequencer::Replayed;

/// The independently authenticated finalized prefix used by one complete replay pass.
#[derive(Debug)]
struct AuditTrust {
    parent: CheckpointParent,
    authorities: Vec<AcceptedCheckpoint>,
}

/// Verifies a public log against configured custody and an independently trusted current signer.
/// Error cases:
/// - custody configuration, canonical RPC reads, accepted history, or the log fails verification
/// - the supplied signer differs from finalized custody, or the log omits its finalized anchor
pub async fn replay_path_with_custody(path: &Path, cfg: &NodeConfig, signer: AccountId) -> anyhow::Result<Replayed> {
    ensure!(cfg.custody.provider == CustodyProvider::Evm, "custody-backed audit requires EVM custody");
    let checkpoints = cfg.checkpoints.as_ref().context("custody-backed audit requires [checkpoints]")?;
    let evm = cfg.custody.evm.as_ref().context("custody-backed audit requires [custody.evm]")?;
    let domain = CheckpointDomain {
        chain_id: cfg.custody.chain_id,
        vault: cfg.custody.vault.parse().context("custody audit vault address")?,
    };
    let chain = CheckpointRpc::read_only(checkpoints, domain, evm.usdc, evm.finality_tag, evm.confirmations)?;
    replay_with_chain(path, &chain, domain, signer).await
}

/// Builds independently authenticated trust before applying the log through ordinary strict replay.
/// Error cases:
/// - finalized custody, accepted history, or the complete replay pass fails verification
async fn replay_with_chain(
    path: &Path,
    chain: &impl CheckpointChain,
    domain: CheckpointDomain,
    signer: AccountId,
) -> anyhow::Result<Replayed> {
    let trust = build_trust(path, chain, domain, signer).await?;
    crate::sequencer::replay_path_with_authorities(path, domain, signer, trust.parent, trust.authorities)
}

/// Authenticates only exact accepted checkpoint identities at one canonical finalized block.
/// Error cases:
/// - signer, snapshot, log, finalized anchor, accepted identity, or signature is inconsistent
async fn build_trust(
    path: &Path,
    chain: &impl CheckpointChain,
    domain: CheckpointDomain,
    signer: AccountId,
) -> anyhow::Result<AuditTrust> {
    ensure!(signer.0 != [0; 20], "custody audit requires a nonzero independently trusted signer");
    let at = chain.observe(View::Finalized, &[]).await?;
    ensure!(at.view == View::Finalized, "custody audit requires a finalized observation");
    ensure!(at.state.signer == signer, "finalized vault signer differs from the supplied trusted signer");
    at.validate(&[])?;
    let evidence = at.lifecycle.as_ref().context("custody audit requires canonical lifecycle evidence")?;
    let parent = at
        .state
        .checkpoint
        .as_ref()
        .map(|(seq, root)| -> anyhow::Result<_> {
            Ok(CheckpointParent { has_checkpoint: true, seq: *seq, root: root.parse()? })
        })
        .transpose()?
        .unwrap_or_default();
    let candidates = checkpoint_candidates(path, parent)?;
    let mut authorities = Vec::with_capacity(candidates.len());
    for page in candidates.chunks(APPROVAL_PAGE_SIZE) {
        let sequences = page.iter().map(|checkpoint| checkpoint.seq).collect::<Vec<_>>();
        let accepted = chain.accepted_checkpoints(&at, &sequences).await?;
        ensure!(accepted.len() == page.len(), "vault lacks accepted checkpoint signer history");
        for (authority, checkpoint) in accepted.into_iter().zip(page) {
            authority.verify(checkpoint)?;
            ensure!(authority.signer_epoch <= evidence.signer_epoch, "accepted checkpoint has a future signer epoch");
            signature::verify_domain(checkpoint, domain)?;
            authorities.push(authority);
        }
    }
    Ok(AuditTrust { parent, authorities })
}

/// Reads candidate identities without opening a writer, repairing a manifest, or trusting the log.
/// Error cases:
/// - the path or manifest cannot be read, or the exact finalized checkpoint is absent
fn checkpoint_candidates(path: &Path, parent: CheckpointParent) -> anyhow::Result<Vec<Checkpoint>> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let mut candidates = Vec::new();
    let mut anchor_found = !parent.has_checkpoint;
    let mut visit = |entry| {
        if let LogEntry::Checkpoint(checkpoint) = entry
            && checkpoint.parent.is_some()
            && parent.has_checkpoint
            && checkpoint.seq <= parent.seq
        {
            if checkpoint.seq == parent.seq {
                ensure!(
                    checkpoint.root.parse::<engine_core::eip712::B256>()? == parent.root,
                    "public log differs from the finalized vault anchor"
                );
                anchor_found = true;
            }
            candidates.push(checkpoint);
        }
        Ok(())
    };
    if found.is_dir() {
        SegmentedLog::open(path, &mut visit)?;
    } else {
        log::read_file(path, &mut visit)?;
    }
    ensure!(anchor_found, "public log is shorter than or omits the finalized vault anchor");
    Ok(candidates)
}

#[cfg(test)]
mod tests;
