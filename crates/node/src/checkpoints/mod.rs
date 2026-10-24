//! On-chain checkpoint commitments, replay protection, and collateral reconciliation.

pub(crate) mod accounting;
mod lifecycle;
pub(crate) use lifecycle::{APPROVAL_PAGE_SIZE, AcceptedCheckpoint, ApprovalRegistry, Observation, SignerHistory};
pub use lifecycle::{ApprovalId, ApprovalStatus, LifecycleEvidence};
mod rpc;
pub(crate) mod setup;
pub(crate) mod signature;
pub(crate) use rpc::CheckpointRpc;

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, ensure};
use engine_core::Fixed;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{Instrument, error, warn};

use crate::checkpoints::accounting::Accounting;
use crate::log::{Checkpoint, CheckpointDomain};
use crate::sequencer::Request;

/// A signed checkpoint and the custody liabilities its replayed state carries.
#[derive(Clone, Debug)]
pub(crate) struct Work {
    pub checkpoint: Checkpoint,
    pub liabilities: Fixed,
    pub deposits: u64,
    pub authorized: Fixed,
}

impl Work {
    /// Captures custody accounting without traversing engine accounts or credited references.
    pub(crate) fn capture(checkpoint: Checkpoint, accounting: Accounting) -> Self {
        Self {
            checkpoint,
            liabilities: accounting.liabilities,
            deposits: accounting.deposits,
            authorized: accounting.unpaid(),
        }
    }
}

/// Durable work registered by its sequencer before bounded queue admission.
pub(crate) type Registry = Arc<Mutex<BTreeMap<u64, Work>>>;

/// The latest commitment and collateral, read at one canonical block hash.
#[derive(Clone, Debug)]
pub(crate) struct VaultState {
    pub height: u64,
    pub signer: engine_core::types::AccountId,
    pub checkpoint: Option<(u64, String)>,
    pub balance: Fixed,
    pub deposits: u64,
}

/// Latest inclusion is progress; only final custody-policy state authorizes signature release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    Latest,
    Finalized,
}

/// Chain access, kept separate from the authorized enclave signer lifecycle.
pub(crate) trait CheckpointChain: Send + Sync {
    /// Reads the authenticated crash-recovery reservation without persisted mutation.
    /// Error cases:
    /// - the existing submission record is malformed or unauthorized
    fn reserved_checkpoint(&self) -> anyhow::Result<Option<Checkpoint>> {
        Ok(None)
    }

    /// Reads the authorized signer, latest checkpoint, and collateral from one block.
    /// Error cases:
    /// - an RPC call fails, decodes incorrectly, or serves another chain
    async fn state(&self, view: View) -> anyhow::Result<VaultState>;
    /// Reads lifecycle metadata together with collateral at one canonical block.
    /// Error cases:
    /// - the state read fails or the approval page exceeds its bounded size
    async fn observe(&self, view: View, approvals: &[ApprovalId]) -> anyhow::Result<Observation> {
        ensure!(approvals.len() <= APPROVAL_PAGE_SIZE, "approval query exceeds page size");
        Ok(Observation { view, state: self.state(view).await?, lifecycle: None })
    }

    /// Authenticates accepted identities against the already sampled canonical observation.
    /// Error cases:
    /// - historical metadata is unavailable or the requested page exceeds its bound
    async fn accepted_checkpoints(
        &self,
        at: &Observation,
        sequences: &[u64],
    ) -> anyhow::Result<Vec<AcceptedCheckpoint>> {
        ensure!(sequences.len() <= APPROVAL_PAGE_SIZE, "checkpoint authority query exceeds page size");
        ensure!(at.view == View::Finalized, "historical authority requires a finalized observation");
        sequences
            .iter()
            .map(|seq| {
                let (accepted, root) = at.state.checkpoint.as_ref().context("vault has no accepted checkpoint")?;
                ensure!(seq == accepted, "adapter cannot authenticate historical checkpoint authorities");
                Ok(AcceptedCheckpoint {
                    seq: *seq,
                    root: root.clone(),
                    signer: at.state.signer,
                    signer_epoch: at.lifecycle.as_ref().map_or(1, |evidence| evidence.signer_epoch),
                })
            })
            .collect()
    }

    /// Reads immutable signer epochs at the already pinned finalized block, in query order.
    /// Error cases:
    /// - the adapter cannot authenticate an epoch, or the query exceeds its work bound
    async fn epoch_signers(
        &self,
        at: &Observation,
        epochs: &[u64],
    ) -> anyhow::Result<Vec<engine_core::types::AccountId>> {
        ensure!(epochs.len() <= APPROVAL_PAGE_SIZE, "signer epoch query exceeds page size");
        ensure!(at.view == View::Finalized, "signer history requires a finalized observation");
        let evidence = at.lifecycle.as_ref().context("signer history requires canonical lifecycle evidence")?;
        epochs
            .iter()
            .map(|epoch| {
                ensure!(*epoch == evidence.signer_epoch, "adapter cannot authenticate retired signer epochs");
                Ok(evidence.signer)
            })
            .collect()
    }

    /// Relays a signed checkpoint. Receipt loss is safe: the next state read detects the post.
    /// Error cases:
    /// - submission fails, times out, or reverts
    async fn post(&self, checkpoint: &Checkpoint) -> anyhow::Result<()>;
}

#[derive(Debug)]
struct Conflict(&'static str);

impl std::fmt::Display for Conflict {
    /// The commitment invariant that failed.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for Conflict {}

/// Supervised checkpoint worker; work is durable in the log before it reaches this queue.
pub(crate) struct Poster<R> {
    pub chain: R,
    pub registry: Option<Registry>,
    pub domain: CheckpointDomain,
    pub signer: engine_core::types::AccountId,
    pub retry: Duration,
    pub pending_deposits_critical_after: Duration,
}

impl<R: CheckpointChain> Poster<R> {
    /// Checks the chain anchor before recovery opens the log for writing or binds any listener.
    /// Error cases:
    /// - reading fails, the authorized signer differs, or the log does not contain the posted root
    pub async fn startup(&self, anchor: Option<&Checkpoint>) -> anyhow::Result<()> {
        let at = self.chain.observe(View::Finalized, &[]).await?;
        at.validate(&[])?;
        let state = &at.state;
        ensure!(state.signer == self.signer, "vault signer differs from this enclave; signer recovery is required");
        if let Some((seq, root)) = &state.checkpoint {
            let cp = anchor.context("log is shorter than the latest posted checkpoint")?;
            ensure!(cp.seq == *seq && cp.root == *root, "log differs from the latest posted checkpoint at seq {seq}");
            signature::verify_domain(cp, self.domain)?;
            let accepted = self.chain.accepted_checkpoints(&at, &[*seq]).await?;
            ensure!(accepted.len() == 1, "vault did not authenticate the posted checkpoint");
            accepted[0].verify(cp)?;
        }
        Ok(())
    }

    /// Posts checkpoints in log order, retries transport failures, and reports withdrawal health.
    /// Error cases:
    /// - the sequencer disappears, or the chain exposes an incompatible checkpoint/signer
    #[cfg(test)]
    pub async fn run(self, rx: mpsc::Receiver<Work>, tx: mpsc::Sender<Request>) -> anyhow::Result<()> {
        self.run_with_recovery(rx, tx, Vec::new()).await
    }

    /// Restores the durable signed tail before accepting newly sequenced checkpoints.
    /// Error cases:
    /// - the sequencer disappears or authenticated chain state conflicts
    #[cfg(test)]
    pub async fn run_with_recovery(
        self,
        rx: mpsc::Receiver<Work>,
        tx: mpsc::Sender<Request>,
        recovery: Vec<Work>,
    ) -> anyhow::Result<()> {
        self.run_with_approvals(rx, tx, recovery, Default::default()).await
    }

    /// Polls bounded pages of replay-registered approvals while extending the checkpoint chain.
    /// Error cases:
    /// - the sequencer disappears, registry is poisoned, or authenticated state conflicts
    pub async fn run_with_approvals(
        self,
        mut rx: mpsc::Receiver<Work>,
        tx: mpsc::Sender<Request>,
        recovery: Vec<Work>,
        approvals: ApprovalRegistry,
    ) -> anyhow::Result<()> {
        let mut approval_cursor = None;
        let mut queued = VecDeque::from(recovery);
        let mut latest = None;
        let mut pending_since = None;
        let mut timer = interval(self.retry);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                work = rx.recv(), if queued.len() < 64 => match work {
                    Some(work) => queued.push_back(work),
                    None => return Ok(()),
                },
                _ = timer.tick() => {
                    let work = queued.front().or(latest.as_ref());
                    let Some(work) = work else { continue };
                    let registered = self.registry.as_ref().map(|registry| registry.lock()
                        .map(|work| work.values().cloned().collect::<Vec<_>>())
                        .map_err(|_| anyhow::anyhow!("checkpoint work registry is poisoned")))
                        .transpose()?.unwrap_or_default();
                    let mut known: Vec<&Work> = queued.iter().chain(registered.iter()).collect();
                    if let Some(latest) = latest.as_ref() { known.push(latest); }
                    // Identity belongs to context, not event fields: alert fields key episodes.
                    let span = tracing::info_span!("checkpoint", checkpoint_seq = work.checkpoint.seq,
                        checkpoint_root = %work.checkpoint.root, latest_seq = tracing::field::Empty,
                        finalized_seq = tracing::field::Empty);
                    let page = approval_page(&approvals, approval_cursor)?;
                    let result = self.reconcile_lifecycle(work, &known, &page).instrument(span.clone()).await;
                    if matches!(result, Ok((Some(_), false, _))) {
                        let since = pending_since.get_or_insert_with(tokio::time::Instant::now);
                        if since.elapsed() >= self.pending_deposits_critical_after {
                            span.in_scope(|| error!(alert = "collateral_mismatch", "withdrawals paused: collateral reconciliation exceeded its catch-up deadline"));
                        }
                    } else if matches!(result, Ok((_, true, _))) {
                        pending_since = None;
                    }
                    let (posted, healthy, lifecycle) = match result {
                        Ok(status) => status,
                        Err(error) => {
                            if error.is::<Conflict>() {
                                span.in_scope(|| error!(alert = "checkpoint_conflict", "refusing to continue: {error:#}"));
                                return Err(error);
                            }
                            span.in_scope(|| warn!(alert = "checkpoint_post_failed", "checkpoint remains queued: {error:#}"));
                            (None, false, None)
                        }
                    };
                    if let Some(evidence) = &lifecycle {
                        approval_cursor = page.last().map(|approval| (approval.account, approval.nonce));
                        let mut registered = approvals.lock().map_err(|_| anyhow::anyhow!("approval registry is poisoned"))?;
                        for status in &evidence.approvals {
                            if status.claimed && registered.get(&(status.approval.account, status.approval.nonce)) == Some(&status.approval) {
                                registered.remove(&(status.approval.account, status.approval.nonce));
                            }
                        }
                    }
                    tx.send(Request::CheckpointStatus { posted, healthy, lifecycle }).await.map_err(|_| anyhow::anyhow!("sequencer stopped"))?;
                    if let Some(posted) = posted {
                        let confirmed = known.iter().find(|work| work.checkpoint.seq == posted).map(|work| (*work).clone());
                        while queued.front().is_some_and(|work| work.checkpoint.seq <= posted) { queued.pop_front(); }
                        if let Some(confirmed) = confirmed { latest = Some(confirmed); }
                        if let Some(registry) = &self.registry {
                            registry.lock().map_err(|_| anyhow::anyhow!("checkpoint work registry is poisoned"))?
                                .retain(|seq, _| *seq >= posted);
                        }
                    }
                }
            }
        }
    }
}

mod reconciliation;

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod scenario;
#[cfg(test)]
mod tests;

/// Selects one bounded ordered page without cloning the full approval registry.
/// Error cases:
/// - the registry lock is poisoned
fn approval_page(
    registry: &ApprovalRegistry,
    cursor: Option<(engine_core::types::AccountId, u64)>,
) -> anyhow::Result<Vec<ApprovalId>> {
    use std::ops::Bound::{Excluded, Unbounded};
    let registered = registry.lock().map_err(|_| anyhow::anyhow!("approval registry is poisoned"))?;
    let page: Vec<_> = registered
        .range((cursor.map_or(Unbounded, Excluded), Unbounded))
        .take(APPROVAL_PAGE_SIZE)
        .map(|(_, approval)| *approval)
        .collect();
    Ok(if page.is_empty() { registered.values().take(APPROVAL_PAGE_SIZE).copied().collect() } else { page })
}
