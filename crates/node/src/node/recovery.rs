//! Reading the log and the history index it must match before anything is written.

use std::sync::Arc;

use anyhow::Context;
use engine_core::types::AccountId;
use tracing::error;

use crate::checkpoints::setup::Setup;
use crate::config::NodeConfig;
use crate::history::HistoryPlan;
use crate::log::Checkpoint;
use crate::log_heads::StartupHeads;
use crate::log_heads::setup::Pending;
use crate::metrics::Metrics;
use crate::node::marks;
use crate::node::member::Replica;
use crate::node::ready::Ready;
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
/// disk, replayed in full against the signer history `setup` reads.
/// Error cases:
/// - see `from_replica` and `read_log_and_history_authorities`, or the signer history or the
///   dashboard record cannot be read
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
        let (log, pending) = from_replica(cfg, key, parent, replica.follower, ready.heads)?;
        let anchor = ready.anchor.clone();
        return Ok(Recovered { log, history: replica.history, pending, anchor, stats: replica.stats });
    }
    let marks = marks.shared.as_ref().expect("the marks store opens at startup");
    let authorities = super::signer_history::load(setup, &cfg.node.log_dir)
        .await
        .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))?;
    // The dashboard record recovery rebuilds from the log's replay and the sequencer then continues.
    let stats = Stats::new(cfg).context("stats")?;
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
        read_log_and_history_authorities(cfg, key, parent, authorities, marks, &stats, observe).await?;
    // Counted once it has run, as every full replay is: a snapshot's start replays only the suffix
    // past it, and one that fell back replayed the whole log.
    if log.snapshot_seq.is_none() {
        metrics.full_replays.inc();
    }
    Ok(Recovered { log, history, pending, anchor, stats })
}

/// A member's replica as the recovered log, finished at the vault's finalized checkpoint `parent`
/// under this node's key, with the posted log heads `heads` the member read before it connected.
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
    heads: Option<StartupHeads>,
) -> anyhow::Result<(ReplayedLog, Option<Pending>)> {
    let pending = heads.map(|heads| crate::log_heads::setup::pending(cfg, key, heads)).transpose()?;
    let posted = pending.as_ref().and_then(Pending::posted).map(|head| head.seq);
    let log = follower
        .into_log(parent, AccountId(key.address()), posted)
        .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    Ok((log, pending))
}

/// Replays with independently finalized historical signer evidence before any persisted mutation,
/// from a verified snapshot when snapshots are configured, seeding the marks cache through `marks`.
/// The on-chain posted log head is read before the log replays: the rollback check binds to the
/// link the replay captures at the posted seq, never a second pass over the host's files.
/// Error cases:
/// - history, log, authenticated signer history, the posted-head read, or the marks cache fails
pub(super) async fn read_log_and_history_authorities(
    cfg: &NodeConfig,
    key: &Arc<EnclaveKey>,
    parent: Option<crate::log::CheckpointParent>,
    authorities: crate::checkpoints::SignerHistory,
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
        let (log, plan) =
            Sequencer::read_paired_log(cfg, signer, parent, authorities, Some(stats.clone()), capture, observe)
                .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
        return Ok((log, HistoryPlan::authenticated(plan)?, pending));
    }
    let history_failed = |e: &anyhow::Error| error!(alert = "history_recovery_failed", "refusing to serve: {e:#}");
    let mut check = HistoryPlan::open(&cfg.history).inspect_err(history_failed)?;
    let (sink, seeding, reset_done) = marks::recovery_sink(marks);
    let log = Sequencer::read_log_with_authorities(
        &cfg.node.log_dir,
        Some(crate::log::CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? }),
        signer,
        parent,
        authorities,
        |entry| {
            check.observe(entry);
            observe(entry);
        },
        Some(sink),
        Some(stats.clone()),
        capture.map(|posted| posted.seq),
    )
    .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    // A replay that never moved a mark still resets: the cache must not serve another log's rows.
    if !reset_done.get() {
        marks.borrow_mut().reset_log_derived().context("resetting the marks cache for re-derivation")?;
    }
    anyhow::ensure!(!seeding.get(), "seeding the marks cache from the log failed");
    Ok((log, check, pending))
}

#[cfg(test)]
mod tests;
