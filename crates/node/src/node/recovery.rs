//! Reading the log and the history index it must match before anything is written.

use std::sync::Arc;

use anyhow::Context;
use engine_core::types::AccountId;
use tracing::{error, warn};

use crate::checkpoints::setup::Setup;
use crate::config::NodeConfig;
use crate::daily_snapshots::History;
use crate::history::HistoryPlan;
use crate::log::Checkpoint;
use crate::log_heads::StartupHeads;
use crate::log_heads::setup::Pending;
use crate::metrics::Metrics;
use crate::node::marks;
use crate::node::member::Replica;
use crate::node::ready::Ready;
use crate::sequencer::replay::SnapshotRefused;
use crate::sequencer::{ReplayedLog, Sequencer};
use crate::stats::Stats;
use crate::tee::EnclaveKey;

/// The log a start recovered, with what it checks and continues beside it.
pub(super) struct Recovered {
    pub log: ReplayedLog,
    pub history: HistoryPlan,
    /// The posted log head's startup check and poster, with `[log_heads]`.
    pub pending: Option<Pending>,
    /// The checkpoint at the vault's finalized seq, as the log holds it.
    pub anchor: Option<Checkpoint>,
    /// The dashboard record the replay fed.
    pub stats: Stats,
}

/// The log this start runs from: a member's replica, which the tail handover left at the predecessor's
/// final checkpoint, finished with what the member read before it connected (`ready`), or the log on
/// disk, from a cache or replayed in full against the signer history `setup` reads.
/// Error cases:
/// - see `from_replica` and `read_log_and_history_authorities`, or the dashboard record cannot be read
pub(super) async fn recover(
    cfg: &NodeConfig,
    key: &Arc<EnclaveKey>,
    setup: Option<&Setup>,
    marks: &marks::MarksStartup,
    replica: Option<(Replica, &Ready)>,
    metrics: &Metrics,
) -> anyhow::Result<Recovered> {
    let posted_seq = setup.and_then(|setup| setup.posted_seq);
    if let Some((replica, ready)) = replica {
        let parent = setup.map(|setup| setup.parent);
        let promoted = ready.promoted.is_some();
        let (log, pending) = from_replica(cfg, key, parent, replica.follower, (ready.heads, promoted))?;
        let anchor = ready.anchor.clone();
        return Ok(Recovered { log, history: replica.history, pending, anchor, stats: replica.stats });
    }
    // A start from a daily snapshot reads no log before its checkpoint; none means a whole replay. Without
    // `[snapshots]` it is chosen now, so the dashboard record and the marks cache start from the history it carries;
    // with them a recovery cache comes first, and one is chosen only for the cache's pass over the log's prefix,
    // the record and the cache then starting at the cache's, or at the log's start when no cache is taken.
    let mut start = match cfg.snapshots {
        Some(_) => None,
        None => snapshot::chosen(cfg, key, setup).await,
    };
    // The dashboard record recovery rebuilds from the log's replay and the sequencer then continues.
    let stats = Stats::new(cfg).context("stats")?;
    // From a daily snapshot, the record and the marks cache start at its checkpoint, from the history it carries.
    match start.as_mut() {
        Some(start) => {
            let history = History::read(start.snapshot.history.take().as_deref());
            match history.stats {
                Some(record) => stats.restore(record),
                None => stats.restart(),
            }
            marks.import_frozen(start.snapshot.offset, history.marks.as_ref());
        }
        None => {
            marks.import_live().inspect_err(|e| error!(alert = "marks_cache_failed", "refusing to serve: {e:#}"))?
        }
    }
    let marks = marks.shared.as_ref().expect("the marks store opens at startup");
    let start = match cfg.snapshots {
        Some(_) => futures_util::future::Either::Left(snapshot::chosen(cfg, key, setup)),
        None => futures_util::future::Either::Right(std::future::ready(start)),
    };
    // Read only for a replay of the log, from its start or the snapshot's: a cache's suffix replays under
    // this node's key alone.
    let authorities = async |from| {
        super::signer_history::load(setup, &cfg.node.log_dir, from)
            .await
            .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))
    };
    let mut anchor = None;
    let observe = |entry: &crate::log::LogEntry| {
        if let crate::log::LogEntry::Checkpoint(cp) = entry
            && posted_seq == Some(cp.seq)
        {
            anchor = Some(cp.clone());
        }
    };
    let parent = setup.map(|setup| setup.parent);
    let (log, history, pending) =
        read_log_and_history_authorities(cfg, key, (parent, start), authorities, marks, &stats, observe).await?;
    // Counted once it has run, as every full replay is: a snapshot's start replays only the suffix
    // past it, and one that fell back replayed the whole log.
    if log.snapshot_seq.is_none() {
        metrics.full_replays.inc();
    }
    // A vault checkpoint the log holds no entry for anchors as the replay found it (#595).
    let unheld = log.replayed.recovery_anchor_work.as_ref().map(|work| &work.checkpoint);
    let anchor = anchor.or_else(|| unheld.filter(|checkpoint| Some(checkpoint.seq) == posted_seq).cloned());
    Ok(Recovered { log, history, pending, anchor, stats })
}

/// A member's replica as the recovered log, finished at the vault's finalized checkpoint `parent`
/// under this node's key, with the posted log heads `heads` the member read before it connected; a
/// promoted member's (`promoted`) under the standby key the promotion installed, which has signed no
/// checkpoint yet.
/// Nothing replays or reads the chain. The marks cache keeps the log-derived rows it holds from this
/// member's earlier holdings, extended by the sequencer's live reports from here.
/// Error cases:
/// - the replica does not finish (see `LiveReplay::into_log`), or the log heads' poster cannot be
///   built
fn from_replica(
    cfg: &NodeConfig,
    key: &Arc<EnclaveKey>,
    parent: Option<crate::log::CheckpointParent>,
    follower: crate::standby::Follower,
    (heads, promoted): (Option<StartupHeads>, bool),
) -> anyhow::Result<(ReplayedLog, Option<Pending>)> {
    let pending = heads.map(|heads| crate::log_heads::setup::pending(cfg, key, heads)).transpose()?;
    let posted = pending.as_ref().and_then(Pending::posted).map(|head| head.seq);
    let signer = AccountId(key.address());
    let log = match promoted {
        true => follower.into_promoted_log(parent, signer, posted),
        false => follower.into_log(parent, signer, posted),
    };
    let log = log.inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    Ok((log, pending))
}

/// Replays with independently finalized historical signer evidence before any persisted mutation,
/// from a verified recovery cache when `[snapshots]` is configured, or from the daily snapshot `start` gives,
/// if any, seeding the marks cache through `marks`: re-derived from the log's start, or extended from the
/// snapshot's, where the cache and `stats` stand at its checkpoint already. `start` is awaited only for a replay
/// of the log, or for a cache's pass over the log's prefix, which may start from it. `authorities` gives that
/// evidence, from the daily snapshot's offset and checkpoint when one is taken, awaited only for a replay of the
/// log. The on-chain posted log head is read before the log replays: the rollback check binds to the link the
/// replay captures at the posted seq, never a second pass over the host's files, or, for a head at or under the
/// daily snapshot's checkpoint, to the history the snapshot carries. A daily snapshot the replay cannot go on from at
/// all (`SnapshotRefused`) is paged as Warning `snapshot_start_refused`, and the whole log replays instead.
/// Error cases:
/// - history, log, authenticated signer history, the posted-head read or its binding, or the marks cache fails
pub(super) async fn read_log_and_history_authorities(
    cfg: &NodeConfig,
    key: &Arc<EnclaveKey>,
    (parent, start): (
        Option<crate::log::CheckpointParent>,
        impl Future<Output = Option<crate::daily_snapshots::Signed>>,
    ),
    authorities: impl AsyncFn(
        Option<(crate::log::Offset, crate::log::Checkpoint)>,
    ) -> anyhow::Result<crate::checkpoints::SignerHistory>,
    marks: &std::rc::Rc<std::cell::RefCell<crate::history::marks::Store>>,
    stats: &Stats,
    mut observe: impl FnMut(&crate::log::LogEntry),
) -> anyhow::Result<(ReplayedLog, HistoryPlan, Option<crate::log_heads::setup::Pending>)> {
    let signer = Some(AccountId(key.address()));
    // A node that cannot read the posted head cannot bound its log's rollback window: refuse
    // before replaying, let alone writing.
    let pending = crate::log_heads::setup::read_posted(cfg, key)
        .await
        .inspect_err(|e| error!(alert = "log_heads_startup_failed", "refusing to serve: {e:#}"))?;
    let capture = pending.as_ref().and_then(crate::log_heads::setup::Pending::posted);
    if cfg.snapshots.is_some() {
        // Snapshot recovery replays the authenticated suffix only and attaches no marks sink, so the
        // cache keeps the rows it imported and the provider backfill gives the depth they lack.
        let recovery_failed = |e: &anyhow::Error| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}");
        // A cache with no head posted passes over the log's prefix to the vault's finalized checkpoint, from the
        // daily snapshot when there is one.
        let prefix_pass = cfg.log_heads.is_some() && capture.is_none() && parent.is_some_and(|p| p.has_checkpoint);
        let (mut start, mut chosen) = (Some(start), None);
        if prefix_pass && let Some(start) = start.take() {
            chosen = start.await;
        }
        let cached =
            Sequencer::read_paired_cache(cfg, signer, parent, Some(stats), capture, chosen.as_ref(), &mut observe)
                .inspect_err(recovery_failed)?;
        let (log, plan) = match cached {
            Some(recovered) => recovered,
            // With no cache to take, the history index is rebuilt from the whole log, which this start then
            // replays whole too, so the history it indexes is the log it verified.
            None => {
                let (authorities, stats) = (authorities(None).await?, Some(stats.clone()));
                Sequencer::replay_paired_log(cfg, signer, parent, authorities, stats, capture, observe)
                    .inspect_err(recovery_failed)?
            }
        };
        return Ok((log, HistoryPlan::authenticated(plan)?, pending));
    }
    let start = start.await;
    let file = start.as_ref().map(|start| (start.path.clone(), start.snapshot.offset.seq));
    let trust = (signer, parent, capture);
    let (log, check) = match replay(cfg, trust, start, &authorities, marks, stats, &mut observe).await {
        // A snapshot the replay cannot go on from at all leaves the record, the marks cache and the history plan as
        // they were before the replay read any entry: the whole replay starts the record over and re-derives the
        // cache's log-derived rows.
        Err(refused) if refused.is::<SnapshotRefused>() => {
            let (file, seq) = file.expect("only a start from a daily snapshot is refused");
            warn!(
                alert = "snapshot_start_refused",
                seq,
                "the start does not go on from daily snapshot {}: {refused:#}; the whole log replays",
                file.display()
            );
            replay(cfg, trust, None, &authorities, marks, stats, &mut observe).await?
        }
        read => read?,
    };
    Ok((log, check, pending))
}

/// The log replayed without `[snapshots]`, from the daily snapshot `start` when one is given or else from its start,
/// against the signer evidence `authorities` gives from there, with the history plan checked against it; the marks
/// cache's log-derived rows extended from the snapshot's or re-derived from the log's start through `marks`, and the
/// dashboard record `stats` going on from where the caller put it at the snapshot or started over.
/// Error cases:
/// - the snapshot cannot be gone on from at all (`SnapshotRefused`), before any entry is read
/// - the posted head's binding to the snapshot's history, the signer evidence, the history plan, the replay or the
///   marks cache fails, each but the first paged as it fails
async fn replay(
    cfg: &NodeConfig,
    (signer, parent, capture): (
        Option<AccountId>,
        Option<crate::log::CheckpointParent>,
        Option<crate::log_heads::Head>,
    ),
    start: Option<crate::daily_snapshots::Signed>,
    authorities: &impl AsyncFn(
        Option<(crate::log::Offset, crate::log::Checkpoint)>,
    ) -> anyhow::Result<crate::checkpoints::SignerHistory>,
    marks: &std::rc::Rc<std::cell::RefCell<crate::history::marks::Store>>,
    stats: &Stats,
    mut observe: impl FnMut(&crate::log::LogEntry),
) -> anyhow::Result<(ReplayedLog, HistoryPlan)> {
    let from = start.as_ref().map(|start| (start.snapshot.offset, start.snapshot.checkpoint.clone()));
    // A head at or under the snapshot's checkpoint, which the replay after it never reaches, is bound to the history
    // the snapshot carries.
    let posted_ts = match &start {
        Some(start) => crate::log_heads::prefix::under_daily(&cfg.node.log_dir, capture, start)
            .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?,
        None => None,
    };
    let authorities = authorities(from).await?;
    let history_failed = |e: &anyhow::Error| error!(alert = "history_recovery_failed", "refusing to serve: {e:#}");
    let mut check = HistoryPlan::open(&cfg.history).inspect_err(history_failed)?;
    // From a daily snapshot, the replay extends the cache its frozen copy started; from the log's start, it
    // re-derives every log-derived row.
    let (sink, seeding, reset_done) = match start {
        Some(_) => {
            let (sink, seeding) = marks::extending_sink(marks);
            (sink, seeding, None)
        }
        None => {
            let (sink, seeding, reset_done) = marks::recovery_sink(marks);
            (sink, seeding, Some(reset_done))
        }
    };
    let domain =
        Some(crate::log::CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? });
    let (dir, capture, workers) = (&cfg.node.log_dir, capture.map(|posted| posted.seq), cfg.node.replay_workers);
    let observing = |entry: &crate::log::LogEntry| {
        check.observe(entry);
        observe(entry);
    };
    let start_seq = start.as_ref().map(|start| start.snapshot.offset.seq);
    let mut log = match start {
        Some(start) => {
            let trust = (domain, signer, parent);
            let (sink, stats) = (Some(sink), Some(stats.clone()));
            Sequencer::read_log_starting(start, dir, trust, authorities, observing, sink, stats, capture, workers)
        }
        None => Sequencer::read_log_with_authorities(
            dir,
            domain,
            signer,
            parent,
            authorities,
            observing,
            Some(sink),
            Some(stats.clone()),
            capture,
            workers,
        ),
    }
    .inspect_err(|e| {
        if !e.is::<SnapshotRefused>() {
            error!(alert = "log_recovery_failed", "refusing to serve: {e:#}");
        }
    })?;
    log.replayed.ts_at = log.replayed.ts_at.or(posted_ts);
    if let Some(seq) = start_seq {
        check.observe_before(dir, seq);
    }
    // A replay that never moved a mark still resets: the cache must not serve another log's rows.
    if reset_done.is_some_and(|done| !done.get()) {
        marks.borrow_mut().reset_log_derived().context("resetting the marks cache for re-derivation")?;
    }
    anyhow::ensure!(!seeding.get(), "seeding the marks cache from the log failed");
    Ok((log, check))
}

mod snapshot;

#[cfg(test)]
mod history_tests;
#[cfg(test)]
mod tests;
