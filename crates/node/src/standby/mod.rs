//! A standby for the operator in effect: follows its signed stream into a local copy of the
//! published log, ready to take over when the registry's liveness rule promotes this node's
//! candidate. The copy is written in the log's own segment format, so a promoted standby starts
//! from it like any log.
//!
//! Following verifies every frame against the enclave address the operator's verified attestation
//! names (`[standby].enclave`): the host that carries the connection can delay, drop, or splice
//! bytes, but cannot forge a frame. Missed history is backfilled from the operator's public
//! archive on a cold start, and by the stream itself after a restart; the archive's bytes are
//! checked against its manifest, and must replay as the takeover replays the copy before the
//! standby follows onto them (`backfill`).
//!
//! The copy takes only history that extends it (`chain`): each stream resumes at the copy's last
//! command, which the operator must repeat exactly, and each later command must carry the enclave's
//! seal over the link that continues the copy. An operator whose host cut its log back and
//! restarted it fails that check at the first entry, or signed heartbeat, that shows it. The
//! standby then raises `operator_log_rollback`, appends nothing more, and keeps its copy as the
//! source for restoring what the operator lost.
//!
//! A takeover starts when the registry's pending election names this node's admin key and the
//! vault has had no checkpoint for the liveness timeout (a normal election, the incumbent still
//! checkpointing, hands the keys over instead — #44). The standby then finalizes its copy: it
//! refreshes completed segments from the archive in case the last vault-accepted checkpoint
//! posted after the stream last delivered, requires the checkpoint the vault accepted last, and
//! truncates the copy after it, dropping the unsigned tail. It then exits: the operator restarts
//! the node without `--standby`, with `--new-key`, which serves only its attestation until the
//! timelock installs the registered enclave address, recovers the copy, and resumes.

use std::time::Duration;

use anyhow::{Context, bail, ensure};
use engine_core::types::AccountId;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::checkpoints::Takeover;

pub(crate) mod archive;
mod backfill;
pub(crate) mod chain;
mod cut;
pub(crate) use cut::{cut_marked, finish as finish_cut};
mod follow;
use chain::{Chain, rollback_of};
pub(crate) use follow::ReplicaRefused;
use follow::follow;
mod operator;
pub(crate) use operator::{Followed, resolve};
mod stint;
use stint::{StintAction, stint_action};
mod tls;
use tls::tls;
mod replica;
pub(crate) use replica::Follower;

use crate::config::{CustodyProvider, NodeConfig, StandbySection};
use crate::log::{CheckpointDomain, LogEntry, LogWriter, Manifest, Offset, ReplayWorkers, SegmentedLog};
use crate::node::member::turn::COMING_UP;
use crate::telemetry::Telemetry;

/// How long one follow stint runs before the standby re-checks the chain for its promotion: the
/// takeover's worst-case delay after the liveness timeout passes.
const STINT: Duration = Duration::from_secs(15);
/// The most archive bytes one fetch reads, bounding memory for a manifest or segment.
pub(crate) const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
/// The most segments one manifest may list.
pub(super) const MAX_ARCHIVE_LISTINGS: usize = 65_536;
/// How long one archive GET's connection or a WebSocket connect waits.
pub(super) const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one archive body may take end to end.
pub(super) const BODY_TIMEOUT: Duration = Duration::from_secs(300);

/// Runs the standby until the registry promotes this node's candidate, then finalizes the copy
/// and returns for a normal `--new-key` start.
/// Error cases:
/// - the config is not an EVM-custody standby, the copy cannot be opened, the archive bootstrap
///   fails, or the takeover's checkpoint is absent from the copy after an archive refresh
pub async fn run(mut cfg: NodeConfig, mut telemetry: Telemetry, start: crate::KeyStart) -> anyhow::Result<()> {
    let standby = cfg.standby.clone().context("--standby needs a [standby] section")?;
    ensure!(
        cfg.custody.provider == CustodyProvider::Evm && cfg.checkpoints.is_some(),
        "standby mode needs [checkpoints] with EVM custody: it reads the vault and registry"
    );
    // A standby needs its own chain access, and an elected operator brings its own secrets.
    let delivered = crate::node::secrets::startup(&mut cfg, &mut telemetry, &start)
        .await
        .inspect_err(|e| error!("refusing to serve: {e:#}"))?;
    let Telemetry { metrics, alerts, pages_critical } = telemetry;
    if !pages_critical {
        warn!("no alerts.critical webhook: Critical alerts are only logged");
    }
    cfg.alerts.outbox.check_disk_location(&cfg.node.log_dir)?;
    let routing = alerts.spawn(cfg.alerts.episode_quiet, &cfg.alerts.outbox).await?;
    let metrics_listener = crate::metrics::bind(cfg.metrics.listen).await?;
    // A standby relays seeds too: every rotation-set member's node runs the relayer.
    let seed_relay = crate::seed_relay::task(&cfg, &metrics)?;
    let following = async {
        match delivered.listener {
            Some(listener) => tokio::select! {
                outcome = serve(&cfg, &standby) => outcome,
                outcome = listener.run() => outcome,
            },
            None => serve(&cfg, &standby).await,
        }
    };
    // The copy fills the disk while it follows.
    let storage = crate::storage::run(crate::storage::Layout::of(&cfg), cfg.alerts.storage.clone(), metrics.clone());
    let following = async {
        tokio::select! {
            outcome = following => outcome,
            outcome = storage => outcome.and_then(|()| bail!("the disk watcher stopped")),
        }
    };
    let following = async {
        match seed_relay {
            Some(seed_relay) => tokio::select! {
                outcome = following => outcome,
                outcome = seed_relay => outcome,
            },
            None => following.await,
        }
    };
    let outcome = crate::metrics::alongside(metrics_listener, metrics, following).await;
    routing.drain(Duration::from_secs(10)).await;
    outcome
}

/// Follows and takes over; alert routing is the caller's.
/// Error cases:
/// - see `run`
async fn serve(cfg: &NodeConfig, standby: &StandbySection) -> anyhow::Result<()> {
    let my_key: AccountId = cfg.operator()?.address;
    let chain = crate::handover::vault_reader(cfg)?.context("standby mode needs [checkpoints]")?;
    let archive = archive::archive_client(standby.danger_accept_invalid_certs)?;
    // With rotation on, the operator moves with each turn and election: it is read again each stint.
    let mut operator = operator::for_standby(cfg, standby, &chain).await?;
    // A cold start backfills completed segments from the archive; a restart resumes the stream at
    // its own last command, which backfills anything after it.
    let dir = cfg.node.log_dir.as_path();
    let domain =
        CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().context("custody.vault")? };
    // A poisoned cold start must surface now, not when the operator is already gone.
    let snapshots = cfg.daily_snapshots.as_ref().map(|daily| daily.dir.as_path());
    let trust = async || backfill::read_trust(cfg, &chain, &operator, (dir, snapshots), domain).await;
    backfill::prepare(&archive, &operator, dir, (domain, cfg.node.replay_workers), trust).await?;
    let (mut writer, mut copied) =
        open_copy(dir, cfg.node.segment_commands.max(1), Chain::new(domain, operator.enclave))?;
    info!(operator = %operator.origin, enclave = %operator.enclave, "standby following the operator in effect");
    // When the followed operator last moved: its successor may take a while to serve.
    let mut moved_at: Option<std::time::Instant> = None;
    // The operator each open episode paged, which its resolution names: a resolution under another
    // key would end no episode.
    let mut follow_failed: Option<String> = None;
    // Unknown at a start, which may find an episode the alert outbox kept open: the first read ends it.
    let mut signal_failed: Option<bool> = None;
    let mut rolled_back: Option<String> = None;
    loop {
        match chain.takeover(my_key).await {
            Ok(Some(takeover)) => {
                // Read while the copy is still open, so a failed read retries at the next poll.
                let Some(signers) = operator::takeover_signers(cfg, &chain, &operator, &mut signal_failed).await else {
                    sleep(standby.poll_interval).await;
                    continue;
                };
                writer.flush().context("flushing the copy before the takeover")?;
                drop(writer);
                let checked = (domain, signers, cfg.node.replay_workers);
                let finalized =
                    finalize(&archive, checked, &operator.origins(), (dir, snapshots), &takeover, copied).await;
                if let Err(e) = finalized {
                    error!(alert = "standby_takeover_failed", "the takeover could not finalize: {e:#}");
                    return Err(e);
                }
                info!(
                    alert_resolved = "standby_promoted",
                    seq = takeover.seq,
                    "promoted: the copy ends at the latest published checkpoint the outgoing key \
                     signed; start this node without --standby, with --new-key, under a config \
                     naming the operator in effect's key, and have the timelock install this \
                     enclave's registered address"
                );
                return Ok(());
            }
            Ok(None) => {
                if signal_failed.replace(false) != Some(false) {
                    info!(alert_resolved = "standby_signal_failed", "reading the takeover signal works");
                }
            }
            Err(e) => {
                if signal_failed.replace(true) != Some(true) {
                    error!(alert = "standby_signal_failed", "reading the takeover signal failed: {e:#}");
                }
                sleep(standby.poll_interval).await;
            }
        }
        match operator::for_standby(cfg, standby, &chain).await {
            Ok(resolved) if resolved.moved_from(&operator) => {
                info!(operator = %resolved.origin, enclave = %resolved.enclave, "standby following the operator in effect");
                moved_at = Some(std::time::Instant::now());
                operator = resolved;
            }
            Ok(_) => {}
            Err(e) => warn!("resolving the operator in effect failed; following {} on: {e:#}", operator.origin),
        }
        copied.admit(operator.enclave);
        // The copy's last command is the resume point, so an error mid-stint never duplicates
        // appends: the next stint re-requests from it.
        let followed = follow(&mut writer, &mut copied, cfg, &operator, STINT, None, None).await;
        // The stream itself worked: what it carried is another matter.
        match stint_action(&followed, rolled_back.is_some(), copied.confirmed()) {
            StintAction::FollowFailed => {
                let error = followed.as_ref().unwrap_err();
                // A turn's member serves only once it has taken the keys: no page while it comes up.
                let coming_up = cfg.operator.rotation_enabled
                    && moved_at.is_some_and(|at| at.elapsed() < Duration::from_millis(COMING_UP.unsigned_abs()));
                if coming_up {
                    warn!(operator = %operator.origin, "the operator in effect does not serve yet: {error:#}");
                } else if follow_failed.is_none() {
                    error!(alert = "standby_follow_failed", operator = %operator.origin, "following failed: {error:#}");
                    follow_failed = Some(operator.origin.clone());
                }
                // A dead operator refuses connections at once; the poll interval bounds the retry
                // rate for as long as the liveness window lasts.
                sleep(standby.poll_interval).await;
                continue;
            }
            StintAction::Quiet => {
                if let Some(paged) = follow_failed.take() {
                    info!(alert_resolved = "standby_follow_failed", operator = %paged, "following works again");
                }
                // A host that closes every connection at once must not spin this loop.
                sleep(Duration::from_secs(1)).await;
            }
            StintAction::Rollback(evidence) => {
                if let Some(paged) = follow_failed.take() {
                    info!(alert_resolved = "standby_follow_failed", operator = %paged, "following works again");
                }
                if rolled_back.is_none() {
                    error!(
                        alert = "operator_log_rollback",
                        operator = %operator.origin,
                        "{evidence}; the copy takes nothing more and is kept as the source to restore it from"
                    );
                    rolled_back = Some(operator.origin.clone());
                }
                sleep(standby.poll_interval).await;
            }
            StintAction::Resolved => {
                if let Some(paged) = follow_failed.take() {
                    info!(alert_resolved = "standby_follow_failed", operator = %paged, "following works again");
                }
                if let Some(paged) = rolled_back.take() {
                    info!(
                        alert_resolved = "operator_log_rollback",
                        operator = %paged,
                        "the operator's log holds the copy's history again"
                    );
                }
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Opens the copy for appends, with `empty` taking its entries as the copy's chain, finishing an
/// interrupted cut first: a crash during a takeover's or a replica's cut leaves a `takeover.json`
/// marker (`cut`), and re-running the truncation is idempotent, so the next start finishes the
/// surgery before following again. A copy whose oldest segments are only in the archive is read
/// from the first command the segments on this host hold.
/// Error cases:
/// - the copy cannot be opened, or its links computed, or a marked cut cannot be finished
/// - the segments on this host hold no checkpoint, from which the copy's link is computed
fn open_copy(dir: &std::path::Path, segment_commands: u64, empty: Chain) -> anyhow::Result<(LogWriter, Chain)> {
    let open =
        |from: Option<Offset>| open_copy_replayed(dir, segment_commands, empty.clone(), from, || (), |_, _| Ok(()));
    let opened = open(None).or_else(|e| match e.downcast_ref::<crate::log::NotLocal>() {
        Some(crate::log::NotLocal { first_local: Some(first), .. }) => open(Some(Offset::start_of(*first))),
        _ => Err(e),
    });
    opened.map(|(writer, copied, ())| (writer, copied))
}

/// `open_copy`, also handing each entry to `apply` on what `fresh` starts each attempt with, and
/// reading the copy from the command at `from` when given, from its start otherwise: a replica
/// replays the copy as it is opened, with no second read. A read from a command takes the copy's
/// link from the first checkpoint after it, as the link restarts at each.
/// Error cases:
/// - as `open_copy`, or `apply` refuses an entry
/// - the copy holds no segment from `from` on (`SegmentedLog::open_from`), or no checkpoint after it
fn open_copy_replayed<R>(
    dir: &std::path::Path,
    segment_commands: u64,
    empty: Chain,
    from: Option<Offset>,
    fresh: impl Fn() -> R,
    mut apply: impl FnMut(&mut R, &LogEntry) -> anyhow::Result<()>,
) -> anyhow::Result<(LogWriter, Chain, R)> {
    let mut open = |dir: &std::path::Path| {
        let (manifest_tx, _) = tokio::sync::watch::channel(Manifest::default());
        let mut copied = empty.clone();
        let mut replayed = fresh();
        let mut linked = false;
        let visit = |entry: LogEntry| {
            linked |= matches!(entry, LogEntry::Checkpoint(_));
            apply(&mut replayed, &entry)?;
            copied.record(entry)
        };
        let log = match from {
            Some(from) => SegmentedLog::open_from(dir, from, visit)?,
            None => SegmentedLog::open(dir, visit)?,
        };
        if let Some(from) = from {
            ensure!(linked, "the copy holds no checkpoint from {from} on, from which its link is computed");
        }
        anyhow::Ok((log.into_writer(segment_commands, manifest_tx)?, copied, replayed))
    };
    cut::finish(dir)?;
    open(dir)
}

/// Finalizes the copy for the takeover: refreshes completed segments from the archive in case the
/// vault's last accepted checkpoint posted after the stream last delivered, taking only segments
/// that extend `copied`, then replays the copy in `domain` against `signers`, every command
/// sealed by one of them, its seals checked ahead by `workers`, from the daily snapshot in `snapshots`
/// that `handover::copy_start` gives at or before the takeover's checkpoint, the vault's latest at
/// finality, or whole; requires that checkpoint and truncates the copy after it, dropping the unsigned tail.
/// Error cases:
/// - the archive refresh fails hard, the copy does not replay against `signers`, or it holds no
///   checkpoint at the vault's latest
async fn finalize(
    archive: &reqwest::Client,
    (domain, signers, workers): (CheckpointDomain, std::collections::BTreeSet<AccountId>, ReplayWorkers),
    origins: &[String],
    (dir, snapshots): (&std::path::Path, Option<&std::path::Path>),
    takeover: &Takeover,
    mut copied: Chain,
) -> anyhow::Result<()> {
    // The snapshot is of the copy the stream gave, which the refresh only extends and its undo restores.
    let finalized = crate::handover::Finalized { seq: takeover.seq, root: takeover.root.clone() };
    let start = crate::handover::copy_start(snapshots, dir, (domain, &signers), Some(&finalized)).await;
    // The refresh adds archive bytes no check has replayed yet: until the replay commits them, what the
    // copy held before is recorded, and any start drops what the refresh added.
    backfill::begin_refresh(dir)?;
    let refreshed = archive::refresh_archive(archive, origins, dir, |staged| copied.extend_with(staged)).await;
    match refreshed.as_ref().err().map(|e| (e, rollback_of(e))) {
        Some((_, Some(rollback))) => error!(
            alert = "operator_log_rollback",
            operator = ?origins.last(),
            "{rollback}; the archive's segments are not added to the copy"
        ),
        Some((e, None)) => warn!("refreshing the archive before the takeover failed: {e:#}"),
        None => {}
    }
    // The copy must replay, from a snapshot a recorded signer signed or from its first entry, before any
    // history is dropped: archive-fetched bytes were only hash-checked against the server's own manifest,
    // and this catches a forged prefix here rather than after the timelock installs this enclave.
    // Withdrawals verify against `signers`, the keys that signed this copy's payouts
    // (`operator::copy_signers`), and every command must carry one's seal, so the copy's command prefix is
    // the one the operator posted heads of. A cold start's backfill passes the same replay before the
    // standby follows onto it.
    backfill::replay_refreshed(dir, (domain, workers), signers, start).context("refusing to truncate the copy")?;
    // The resume point is the latest checkpoint in the published log that the outgoing key
    // signed and that extends the vault's latest root: the anchor itself, then every later
    // checkpoint of the copy that continues the outgoing key's signed parent chain. Checkpoints
    // the vault never saw but the outgoing key signed stay: they are published history this
    // node continues, and dropping them could conflict with a relay of the same signatures.
    let resume = resume_checkpoint(dir, takeover)?;
    // The marker makes the surgery resumable: a crash from here is repaired on restart.
    cut::cut_marked(dir, &Takeover { seq: resume.0, root: resume.1, signer: takeover.signer })
        .context("truncating the copy at the takeover")
}

/// The latest published checkpoint the outgoing key signed that extends the vault's latest root:
/// the anchor checkpoint itself, then the newest later copy checkpoint whose signed parent is
/// the running tip. Every skipped or foreign-signed checkpoint between them breaks the chain, so
/// the result is the deepest unbroken continuation. The copy is read from the anchor's seq on, so
/// its segments before the one holding the anchor need not be on this host.
/// Error cases:
/// - the copy cannot be read from the anchor's seq, or holds no checkpoint at the anchor
fn resume_checkpoint(dir: &std::path::Path, takeover: &Takeover) -> anyhow::Result<(u64, String)> {
    let mut tip: Option<(u64, String)> = None;
    let mut found_anchor = false;
    SegmentedLog::open_from(dir, crate::log::Offset::start_of(takeover.seq), |entry| {
        if let LogEntry::Checkpoint(checkpoint) = &entry {
            let root = checkpoint.root.to_lowercase();
            // The signer as replay reads it: a copy may spell an address in either case.
            let by_takeover_signer = checkpoint.signer.parse::<AccountId>().ok() == Some(takeover.signer);
            match &tip {
                None if checkpoint.seq == takeover.seq && root == takeover.root.to_lowercase() => {
                    if !by_takeover_signer {
                        bail!(
                            "the anchor checkpoint at seq {} is signed by {}, not the vault's {}",
                            checkpoint.seq,
                            checkpoint.signer,
                            takeover.signer
                        );
                    }
                    found_anchor = true;
                    tip = Some((checkpoint.seq, root));
                }
                Some((seq, parent_root)) if by_takeover_signer => {
                    if let Some(parent) = &checkpoint.parent
                        && parent.has_checkpoint
                        && parent.seq == *seq
                        && format!("0x{}", hex::encode(parent.root.0)) == *parent_root
                    {
                        tip = Some((checkpoint.seq, root));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })?;
    ensure!(found_anchor, "the copy holds no checkpoint at the vault's latest seq {}", takeover.seq);
    Ok(tip.expect("the anchor set the tip"))
}

/// The WebSocket URL for the stream: the operator's origin with an `http(s)` scheme rewritten to
/// `ws(s)`.
/// Error cases:
/// - the origin is not a URL with an http(s) scheme
fn stream_url(origin: &str, path_and_query: &str) -> anyhow::Result<String> {
    let origin = origin.trim_end_matches('/');
    let url = if origin.starts_with("https://") {
        format!("wss://{}{path_and_query}", origin.strip_prefix("https://").unwrap())
    } else if origin.starts_with("http://") {
        format!("ws://{}{path_and_query}", origin.strip_prefix("http://").unwrap())
    } else {
        bail!("[standby].operator must be an http(s) URL, not {origin:?}");
    };
    Ok(url)
}

/// What shows the operator's log no longer extends the copy, anywhere in `error`'s causes.
pub(crate) fn rollback_evidence(error: &anyhow::Error) -> Option<String> {
    rollback_of(error).map(ToString::to_string)
}

pub(super) fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

#[cfg(test)]
mod copies_tests;
#[cfg(test)]
mod tests;
