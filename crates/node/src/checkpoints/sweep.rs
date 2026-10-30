//! Bounded approval pages and the sweep of further pages at one finalized block.

use anyhow::ensure;
use tokio::sync::mpsc;
use tracing::warn;

use crate::checkpoints::{
    APPROVAL_PAGE_SIZE, ApprovalId, ApprovalRegistry, CheckpointChain, Health, LifecycleEvidence, Poster,
};
use crate::sequencer::Request;

/// Approval pages observed per retry interval, the first with the tick's finalized observation and
/// the rest swept at its block.
pub(super) const SWEEP_PAGES: usize = 64;

type Cursor = Option<(engine_core::types::AccountId, u64)>;

impl<R: CheckpointChain> Poster<R> {
    /// Observes further registry pages at the block of `evidence`, up to `SWEEP_PAGES` pages a
    /// tick, so a restart that registers every historical approval observes them in minutes, not
    /// one page per retry interval. A failed read ends this tick's sweep. Returns the new cursor.
    /// Error cases:
    /// - the registry is poisoned or the sequencer stopped
    pub(super) async fn sweep(
        &self,
        evidence: &LifecycleEvidence,
        approvals: &ApprovalRegistry,
        mut cursor: Cursor,
        posted: Option<u64>,
        health: Option<Health>,
        tx: &mpsc::Sender<Request>,
    ) -> anyhow::Result<Cursor> {
        for _ in 1..SWEEP_PAGES {
            let page = approval_page_after(approvals, cursor)?;
            if page.is_empty() {
                break;
            }
            let statuses = match self.chain.claims(evidence, &page).await.and_then(|statuses| {
                ensure!(
                    statuses.iter().map(|status| status.approval).eq(page.iter().copied()),
                    "nonce-use answers differ from the queried approvals"
                );
                Ok(statuses)
            }) {
                Ok(statuses) => statuses,
                Err(error) => {
                    warn!("approval sweep resumes at the next retry: {error:#}");
                    break;
                }
            };
            cursor = page.last().map(|approval| (approval.account, approval.nonce));
            let lifecycle = Some(LifecycleEvidence { approvals: statuses, ..evidence.clone() });
            tx.send(Request::CheckpointStatus { posted, health, lifecycle })
                .await
                .map_err(|_| anyhow::anyhow!("sequencer stopped"))?;
        }
        Ok(cursor)
    }
}

/// Selects one bounded ordered page, wrapping to the start after the last approval.
/// Error cases:
/// - the registry lock is poisoned
pub(super) fn approval_page(registry: &ApprovalRegistry, cursor: Cursor) -> anyhow::Result<Vec<ApprovalId>> {
    let page = approval_page_after(registry, cursor)?;
    if page.is_empty() { approval_page_after(registry, None) } else { Ok(page) }
}

/// Selects the bounded ordered page after `cursor` without cloning the full approval registry.
/// Error cases:
/// - the registry lock is poisoned
fn approval_page_after(registry: &ApprovalRegistry, cursor: Cursor) -> anyhow::Result<Vec<ApprovalId>> {
    use std::ops::Bound::{Excluded, Unbounded};
    let registered = registry.lock().map_err(|_| anyhow::anyhow!("approval registry is poisoned"))?;
    Ok(registered
        .range((cursor.map_or(Unbounded, Excluded), Unbounded))
        .take(APPROVAL_PAGE_SIZE)
        .map(|(_, approval)| *approval)
        .collect())
}
