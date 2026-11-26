//! A copy of another node's log on this host, as a handover successor and a standby replay it before they trust
//! it: restored from the predecessor's published archive before a handover connects (`sync_predecessor_log`),
//! and replayed from the newest daily snapshot this node holds whose checkpoint the copy holds, rather than from
//! the copy's first entry, so the copy's segments before it need not be on this host (`copy_start`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Context;
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use tracing::{info, warn};

use crate::checkpoints::{CheckpointChain, Observation, VaultState};
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
    /// The key that signed the finalized checkpoint, where it is known, as a takeover's is, or else the vault's signer:
    /// a snapshot of an earlier checkpoint the vault accepted none at rests on it.
    pub signer: AccountId,
    /// The signer of the checkpoint the vault accepted at each seq a copy start may go on from, where it accepted one
    /// there (`daily_snapshots::accepted_signers`).
    pub accepted: BTreeMap<u64, AccountId>,
}

impl AcceptedRoots for Finalized {
    /// The finalized checkpoint's root at its seq. None at an earlier seq: the copy holds the snapshot's
    /// checkpoint there, and the replay from it checks each later checkpoint's root the copy holds.
    async fn root_at(&self, seq: u64) -> Option<B256> {
        (seq == self.seq).then(|| self.root.parse().ok()).flatten()
    }

    /// The signer the vault accepted at `seq`, or else `signer`: a snapshot of a checkpoint the vault accepted none at
    /// that another key signed is passed over, after a signer change before the finalized checkpoint too, and the
    /// copy replays from an earlier one, or whole.
    async fn signer_at(&self, seq: u64) -> Option<AccountId> {
        Some(self.accepted.get(&seq).copied().unwrap_or(self.signer))
    }
}

impl Finalized {
    /// The checkpoint the vault's finalized `state` names, signed by its signer; none before the vault has accepted
    /// one.
    pub(crate) fn of(state: &VaultState) -> Option<Self> {
        let (seq, root) = state.checkpoint.as_ref()?;
        Some(Self { seq: *seq, root: root.clone(), signer: state.signer, accepted: BTreeMap::new() })
    }

    /// The checkpoint a standby takes over at, `takeover`, the vault's latest at finality, signed by its signer, with
    /// the signers the vault `accepted` at earlier seqs.
    pub(crate) fn over(takeover: &crate::checkpoints::Takeover, accepted: BTreeMap<u64, AccountId>) -> Self {
        Self { seq: takeover.seq, root: takeover.root.clone(), signer: takeover.signer, accepted }
    }

    /// The checkpoint `vault`, read at `at`, finalized, with the key that signed it and those it accepted at each
    /// seq a copy start from the daily snapshots in `snapshots` may go on from; none before it has accepted one.
    pub(crate) async fn read(vault: &impl CheckpointChain, at: &Observation, snapshots: Option<&Path>) -> Option<Self> {
        let finalized = Self::of(&at.state)?;
        let signer = crate::daily_snapshots::anchor_signer(vault, at).await;
        Some(Self { signer, ..finalized }.accepting(vault, at, snapshots).await)
    }

    /// `self` with the signers `vault`, read at `at`, accepted at each seq a copy start from the daily snapshots in
    /// `snapshots` may go on from at or before its seq.
    pub(crate) async fn accepting(
        self,
        vault: &impl CheckpointChain,
        at: &Observation,
        snapshots: Option<&Path>,
    ) -> Self {
        let Some(snapshots) = snapshots else { return self };
        let accepted = crate::daily_snapshots::accepted_signers(vault, at, (snapshots, self.seq)).await;
        Self { accepted, ..self }
    }
}

/// The signer of the checkpoint `vault` accepted, at its finalized block, at each seq a copy start from the daily
/// snapshots in `snapshots` may go on from at or before `finalized`; none when that block cannot be read, so each
/// snapshot must be by the finalized checkpoint's signer.
pub(crate) async fn accepted_before(
    vault: &impl CheckpointChain,
    snapshots: Option<&Path>,
    finalized: u64,
) -> BTreeMap<u64, AccountId> {
    let Some(snapshots) = snapshots else { return BTreeMap::new() };
    match vault.observe(crate::checkpoints::View::Finalized, &[]).await {
        Ok(at) => crate::daily_snapshots::accepted_signers(vault, &at, (snapshots, finalized)).await,
        Err(e) => {
            warn!("the vault's accepted checkpoints cannot be read for a copy's daily snapshot: {e:#}");
            BTreeMap::new()
        }
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
/// connects: the completed segments the archive lists, from the bucket `[archive]` names first,
/// checked against their listings. The final segments arrive with the keys instead — the archive
/// serves only completed segments, and this fetches while the predecessor's API is still up. A
/// transport failure is not fatal: a local copy
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
    let mut nodes = section.map(|handover| handover.mirrors.clone()).unwrap_or_default();
    nodes.push(format!("https://{}", source.api_hostname));
    let origins = crate::archive::Origins { bucket: crate::archive::Bucket::for_config(cfg)?, nodes };
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
