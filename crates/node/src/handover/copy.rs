//! A copy of another node's log on this host, as a handover successor and a standby replay it before they trust
//! it: restored from the predecessor's published archive before a handover connects (`sync_predecessor_log`),
//! and replayed from the newest daily snapshot this node holds whose checkpoint the copy holds, rather than from
//! the copy's first entry, so the copy's segments before it need not be on this host (`copy_start`).

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Context;
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use tracing::{info, warn};

use crate::checkpoints::VaultState;
use crate::config::NodeConfig;
use crate::daily_snapshots::{AcceptedRoots, Bounds, Signed, snapshot_start_against};
use crate::handover::Source;
use crate::log::CheckpointDomain;

/// The checkpoint the vault finalized, as its state names it: no daily snapshot after it is one a replay of a
/// copy goes on from, and one at its seq must hold the root the vault accepted there.
#[derive(Debug)]
pub(crate) struct Finalized {
    pub seq: u64,
    pub root: String,
}

impl AcceptedRoots for Finalized {
    /// The finalized checkpoint's root at its seq. None at an earlier seq: the copy holds the snapshot's
    /// checkpoint there, and the replay from it checks each later checkpoint's root the copy holds.
    async fn root_at(&self, seq: u64) -> Option<B256> {
        (seq == self.seq).then(|| self.root.parse().ok()).flatten()
    }
}

impl Finalized {
    /// The checkpoint the vault's finalized `state` names; none before the vault has accepted one.
    pub(crate) fn of(state: &VaultState) -> Option<Self> {
        state.checkpoint.as_ref().map(|(seq, root)| Self { seq: *seq, root: root.clone() })
    }
}

/// The daily snapshot a replay of the copy in `copy` goes on from (`sequencer::replay_copy_from`): the newest of
/// those this node holds in `snapshots`, its `[daily_snapshots]` directory, whose checkpoint the copy holds at or
/// before `finalized`, the vault's finalized checkpoint, signed over its state and what it carries by that
/// checkpoint's signer, one of `signers`, in `domain` (`daily_snapshots::snapshot_start_against`). None without
/// `[daily_snapshots]` or a finalized checkpoint, or when none passes, each refusal paged as Warning
/// `snapshot_start_refused`: the whole copy then replays.
pub(crate) async fn copy_start(
    snapshots: Option<&Path>,
    copy: &Path,
    (domain, signers): (CheckpointDomain, &BTreeSet<AccountId>),
    finalized: Option<&Finalized>,
) -> Option<Signed> {
    let (snapshots, finalized) = (snapshots?, finalized?);
    let bounds = Bounds { finalized: finalized.seq, before: None, keep_from: None };
    snapshot_start_against(None, (snapshots, copy), (domain, signers), bounds, finalized).await
}

/// Fetches the predecessor's published log into this node's log directory before the handover
/// connects: the completed segments the archive lists, checked against their listings. The final
/// segments arrive with the keys instead — the archive serves only completed segments, and this
/// fetches while the predecessor's API is still up. A transport failure is not fatal: a local copy
/// from an earlier start may exist, and the strict tail checks at the handover decide; a fetched
/// copy that fails its integrity checks or does not replay is fatal, surfacing a poisoned mirror
/// before the predecessor stops. The copy replays from the daily snapshot `copy_start` gives under
/// `finalized`, the vault's finalized checkpoint, or whole, which counts in `metrics.full_replays`.
/// Error cases:
/// - the archive fails its integrity checks (`ArchiveIntegrity`), or the restored copy does not replay
///   against `signers`, every command sealed by one of them
pub(crate) async fn sync_predecessor_log(
    cfg: &NodeConfig,
    source: &Source,
    (signers, finalized): (BTreeSet<AccountId>, Option<&Finalized>),
    metrics: &crate::metrics::Metrics,
) -> anyhow::Result<()> {
    let section = cfg.tee.sealing.as_ref().and_then(|sealing| sealing.handover.as_ref());
    let mut origins = section.map(|handover| handover.mirrors.clone()).unwrap_or_default();
    origins.push(format!("https://{}", source.api_hostname));
    let client =
        crate::standby::archive::archive_client(section.is_some_and(|handover| handover.danger_accept_invalid_certs))?;
    let dir = cfg.node.log_dir.as_path();
    // A cut a crash interrupted is finished before the archive extends the copy (see `take_with`).
    crate::standby::finish_cut(dir)?;
    let fetch = if !dir.join(crate::log::MANIFEST).exists() {
        crate::standby::archive::bootstrap_archive(&client, &origins, dir).await
    } else {
        crate::standby::archive::refresh_archive(&client, &origins, dir, |_| Ok(())).await
    };
    if let Err(e) = fetch {
        // Only an integrity failure refuses the start: the archive's bytes or listings fail their
        // checks, so the local prefix cannot be trusted. Any other failure — an origin that is
        // unreachable, slow, or answers an error — is not: a local copy from an earlier attempt may
        // exist, the handover carries the rest, and its replay decides.
        if e.downcast_ref::<crate::standby::archive::ArchiveIntegrity>().is_some() {
            return Err(e).context("the predecessor's published log failed integrity checks");
        }
        warn!("fetching the predecessor's published log failed: {e:#}");
    }
    let manifest = crate::log::Manifest::load(dir)?;
    if manifest.segments.is_empty() {
        // Nothing restored yet — the incumbent's log may still be entirely unsegmented — and the
        // handover carries the whole log.
        return Ok(());
    }
    // A poisoned copy must surface now, not after the predecessor has stopped: archive bytes were
    // only hash-checked against the server's own manifest, and this replay of the restored copy, from
    // a snapshot a recorded signer signed or from its first entry, is the authenticity check of what it
    // reads, as the standby's cold start does. It requires every seal, so the copy's command prefix is the
    // predecessor's; what it does not read, before the snapshot, this node never serves once it sequences.
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let snapshots = cfg.daily_snapshots.as_ref().map(|daily| daily.dir.as_path());
    let start = copy_start(snapshots, dir, (domain, &signers), finalized).await;
    let (_, from) = crate::sequencer::replay_copy_from(start, dir, domain, signers, cfg.node.replay_workers)
        .context("the restored log copy does not replay")?;
    match from {
        Some(seq) => info!(snapshot_seq = seq, "the restored log copy replays to its tip from a daily snapshot"),
        None => {
            metrics.full_replays.inc();
            info!("the restored log copy replays to its tip");
        }
    }
    Ok(())
}
