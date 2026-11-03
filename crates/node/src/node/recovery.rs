//! Reading the log and the history index it must match before anything is written.

use anyhow::Context;
use engine_core::types::AccountId;
use tracing::error;

use crate::config::NodeConfig;
use crate::history::HistoryPlan;
use crate::node::marks;
use crate::sequencer::{ReplayedLog, Sequencer};
use crate::stats::Stats;

/// Replays with independently finalized historical signer evidence before any persisted mutation,
/// from a verified snapshot when snapshots are configured, seeding the marks cache through `marks`.
/// Error cases:
/// - history, log, authenticated signer history, or the marks cache fails validation
pub(super) fn read_log_and_history_authorities(
    cfg: &NodeConfig,
    signer: Option<AccountId>,
    parent: Option<crate::log::CheckpointParent>,
    authorities: crate::checkpoints::SignerHistory,
    marks: &std::rc::Rc<std::cell::RefCell<crate::history::marks::Store>>,
    stats: &Stats,
    mut observe: impl FnMut(&crate::log::LogEntry),
) -> anyhow::Result<(ReplayedLog, HistoryPlan)> {
    if cfg.snapshots.is_some() {
        // Snapshot recovery replays the authenticated suffix only and attaches no marks sink, so the
        // cache keeps the rows it imported and the provider backfill gives the depth they lack.
        let (log, plan) = Sequencer::read_paired_log(cfg, signer, parent, authorities, Some(stats.clone()), observe)
            .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
        return Ok((log, HistoryPlan::authenticated(plan)?));
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
    )
    .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    // A replay that never moved a mark still resets: the cache must not serve another log's rows.
    if !reset_done.get() {
        marks.borrow_mut().reset_log_derived().context("resetting the marks cache for re-derivation")?;
    }
    anyhow::ensure!(!seeding.get(), "seeding the marks cache from the log failed");
    Ok((log, check))
}
