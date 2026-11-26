//! Read-only public-log audits using the vault's canonical finalized signer history.

use std::collections::BTreeSet;
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::types::AccountId;

use crate::checkpoints::{CheckpointChain, CheckpointRpc, SignerHistory, View, accepted_history};
use crate::config::{CustodyProvider, NodeConfig};
use crate::log::{self, Checkpoint, CheckpointDomain, CheckpointParent, LogEntry, SealChecks};
use crate::sequencer::Replayed;
use crate::sequencer::replay::Audited;
use crate::sequencer::replay::ranges::{Trust, replay_ranges};

/// The independently authenticated finalized prefix used by one complete replay pass.
#[derive(Debug)]
struct AuditTrust {
    parent: CheckpointParent,
    authorities: SignerHistory,
}

/// Verifies a public log against configured custody and an independently trusted current signer, its
/// command seals checked as `seals` says.
/// Error cases:
/// - custody configuration, canonical RPC reads, accepted history, or the log fails verification
/// - the supplied signer differs from finalized custody, or the log omits its finalized anchor
pub async fn replay_path_with_custody(
    path: &Path,
    cfg: &NodeConfig,
    signer: AccountId,
    seals: SealChecks,
) -> anyhow::Result<Replayed> {
    let (chain, domain) = custody_chain(cfg)?;
    replay_with_chain(path, &chain, domain, signer, seals).await
}

/// `replay_path_with_custody` in ranges from the daily snapshots in `snapshots`, `jobs` at once, no range
/// starting after the vault's finalized checkpoint (`replay_ranges`).
/// Error cases:
/// - as `replay_path_with_custody`, or the replay in ranges fails (`replay_ranges`)
pub async fn replay_ranges_with_custody(
    path: &Path,
    snapshots: &Path,
    cfg: &NodeConfig,
    signer: AccountId,
    jobs: NonZeroUsize,
) -> anyhow::Result<Audited> {
    let (chain, domain) = custody_chain(cfg)?;
    let AuditTrust { parent, authorities } = build_trust(path, &chain, domain, signer).await?;
    replay_ranges(path, snapshots, Trust::Custody { domain, signer, parent, authorities }, jobs)
}

/// The read-only checkpoint chain and deployment `cfg`'s custody names.
/// Error cases:
/// - the config names no EVM custody or no `[checkpoints]`, or an invalid vault, or the chain client fails
fn custody_chain(cfg: &NodeConfig) -> anyhow::Result<(CheckpointRpc, CheckpointDomain)> {
    ensure!(cfg.custody.provider == CustodyProvider::Evm, "custody-backed audit requires EVM custody");
    let checkpoints = cfg.checkpoints.as_ref().context("custody-backed audit requires [checkpoints]")?;
    let evm = cfg.custody.evm.as_ref().context("custody-backed audit requires [custody.evm]")?;
    let domain = CheckpointDomain {
        chain_id: cfg.custody.chain_id,
        vault: cfg.custody.vault.parse().context("custody audit vault address")?,
    };
    let chain = CheckpointRpc::read_only(checkpoints, domain, evm.usdc, evm.finality_tag, evm.confirmations)?;
    Ok((chain, domain))
}

/// Builds independently authenticated trust before applying the log through ordinary strict replay,
/// its command seals checked as `seals` says.
/// Error cases:
/// - finalized custody, accepted history, or the complete replay pass fails verification
async fn replay_with_chain(
    path: &Path,
    chain: &impl CheckpointChain,
    domain: CheckpointDomain,
    signer: AccountId,
    seals: SealChecks,
) -> anyhow::Result<Replayed> {
    let trust = build_trust(path, chain, domain, signer).await?;
    crate::sequencer::replay_path_with_authorities(path, domain, signer, trust.parent, trust.authorities, seals)
}

/// Authenticates the finalized anchor and the signed-parent chain to it at one canonical block.
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
    at.validate(&[])?;
    // While the guardian has retired the vault's signer, the current epoch's signer is the one
    // that accepted its checkpoints.
    let current = if at.state.signer_retired() {
        let epoch = at.lifecycle.as_ref().context("custody audit requires lifecycle evidence")?.signer_epoch;
        *chain.epoch_signers(&at, &[epoch]).await?.first().context("vault lacks its current signer epoch")?
    } else {
        at.state.signer
    };
    ensure!(current == signer, "finalized vault signer differs from the supplied trusted signer");
    let parent = at
        .state
        .checkpoint
        .as_ref()
        .map(|(seq, root)| -> anyhow::Result<_> {
            Ok(CheckpointParent { has_checkpoint: true, seq: *seq, root: root.parse()? })
        })
        .transpose()?
        .unwrap_or_default();
    let mut epochs = BTreeSet::new();
    let candidates = checkpoint_candidates(path, parent, &mut epochs)?;
    let authorities = accepted_history(
        chain,
        &at,
        domain,
        (parent, CheckpointParent::default(), &BTreeSet::new()),
        &candidates,
        (epochs, BTreeSet::new()),
    )
    .await?;
    Ok(AuditTrust { parent, authorities })
}

/// Reads candidate identities without opening a writer, repairing a manifest, or trusting the log: from a
/// log directory its segments several at once, each parsing only its entries that are not commands
/// (`log::uncommanded`), since the replay that follows checks the whole log.
/// Error cases:
/// - the path or a segment cannot be read, or the exact finalized checkpoint is absent
fn checkpoint_candidates(
    path: &Path,
    parent: CheckpointParent,
    epochs: &mut BTreeSet<u64>,
) -> anyhow::Result<Vec<Checkpoint>> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let mut candidates = Vec::new();
    let mut anchor_found = !parent.has_checkpoint;
    let mut visit = |entry| {
        if let LogEntry::Withdrawal(withdrawal) = &entry
            && let Some(auth) = &withdrawal.authorization
        {
            epochs.insert(auth.signer_epoch);
        }
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
    let mut scanned = Vec::new();
    if found.is_dir() {
        // The scan keeps each candidate and each withdrawal's signer epoch, nothing more, as it reads them.
        let jobs = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
        let keep = |entry| match entry {
            LogEntry::Checkpoint(checkpoint)
                if checkpoint.parent.is_some() && parent.has_checkpoint && checkpoint.seq <= parent.seq =>
            {
                Some(Ok(checkpoint))
            }
            LogEntry::Withdrawal(withdrawal) => withdrawal.authorization.map(|auth| Err(auth.signer_epoch)),
            _ => None,
        };
        for found in log::uncommanded(path, jobs, keep)? {
            match found {
                Ok(checkpoint) => visit(LogEntry::Checkpoint(checkpoint))?,
                Err(epoch) => scanned.push(epoch),
            }
        }
    } else {
        log::read_file(path, &mut visit)?;
    }
    epochs.extend(scanned);
    ensure!(anchor_found, "public log is shorter than or omits the finalized vault anchor");
    Ok(candidates)
}

/// `engine replay`'s replay of the log at `path` under the trust its flags give, `custody_config` with the
/// pinned signer, else the attestations `verified`, else the pinned signer, or none, in `domain`: in ranges
/// from a directory of daily snapshots, so many at once, or whole, its seals checked as `seals` says.
/// Error cases:
/// - the custody config cannot be loaded, or names no signer, or the replay fails
pub fn audit(
    path: &Path,
    ranges: Option<(std::path::PathBuf, NonZeroUsize)>,
    seals: SealChecks,
    custody_config: Option<std::path::PathBuf>,
    (signer, domain): (Option<AccountId>, Option<CheckpointDomain>),
    verified: &[crate::VerifiedAttestation],
) -> anyhow::Result<Audited> {
    if let Some(config) = custody_config {
        let cfg = NodeConfig::load(&config)?;
        let signer = signer.ok_or_else(|| anyhow::anyhow!("--custody-config requires --signer"))?;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        return match ranges {
            Some((dir, jobs)) => runtime.block_on(replay_ranges_with_custody(path, &dir, &cfg, signer, jobs)),
            None => runtime.block_on(replay_path_with_custody(path, &cfg, signer, seals)).map(Audited::from),
        };
    }
    if verified.len() > 1 {
        let attested = verified.iter().map(|v| v.enclave_address).collect();
        return match ranges {
            Some((dir, jobs)) => crate::replay_ranges_with_attested(path, &dir, domain, attested, jobs),
            None => crate::replay_path_with_attested(path, domain, attested, seals).map(Audited::from),
        };
    }
    let trusted = crate::reconcile_signer(signer, verified.first())?;
    match ranges {
        Some((dir, jobs)) => crate::replay_ranges_with_trust(path, &dir, domain, trusted, jobs),
        None => crate::replay_path_with_trust(path, domain, trusted, seals).map(Audited::from),
    }
}

#[cfg(test)]
mod tests;
