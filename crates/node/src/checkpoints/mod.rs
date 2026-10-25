//! On-chain checkpoint commitments, replay protection, and collateral reconciliation.

pub(crate) mod accounting;
pub(crate) mod emission_budget;
mod rpc;
pub(crate) mod setup;
pub(crate) mod signature;
pub(crate) use rpc::CheckpointRpc;

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::emissions::TokenUnits;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{Instrument, error, info, warn};

use crate::checkpoints::accounting::Accounting;
use crate::log::{Checkpoint, CheckpointDomain};
use crate::sequencer::Request;

/// A signed checkpoint and the custody liabilities its replayed state carries.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Work {
    pub checkpoint: Checkpoint,
    pub liabilities: Fixed,
    pub deposits: u64,
    pub coverage: DepositCoverage,
}

/// Historical accounting is replayed unchanged; modern count coverage needs sequenced chain proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DepositCoverage {
    Historical,
    AwaitingCanonicalScan,
    Canonical,
}

impl Work {
    /// Captures custody accounting without traversing engine accounts or credited references.
    pub(crate) fn capture(checkpoint: Checkpoint, accounting: Accounting, engine: &engine_core::Engine) -> Self {
        let (deposits, coverage) = if engine.deposit_finality_enabled() {
            engine
                .custody_deposit_coverage()
                .map_or((accounting.deposits, DepositCoverage::AwaitingCanonicalScan), |count| {
                    (count, DepositCoverage::Canonical)
                })
        } else {
            (accounting.deposits, DepositCoverage::Historical)
        };
        Self { checkpoint, liabilities: accounting.liabilities, deposits, coverage }
    }

    /// Whether this work carries `accounting`'s liabilities and, unless sequenced chain proof
    /// counted them, its deposits.
    pub(crate) fn matches_accounting(&self, accounting: Accounting) -> bool {
        self.liabilities == accounting.liabilities
            && (self.coverage == DepositCoverage::Canonical || self.deposits == accounting.deposits)
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
    /// Relays a signed checkpoint. Receipt loss is safe: the next state read detects the post.
    /// Error cases:
    /// - submission fails, times out, or reverts
    async fn post(&self, checkpoint: &Checkpoint) -> anyhow::Result<()>;

    /// The emission period budget the vault's MONKEY cap was derived from.
    /// Error cases:
    /// - the chain cannot be read, the vault has no MONKEY token, or its cap gives no period budget;
    ///   a chain that reads no cap at all
    async fn emission_budget(&self) -> anyhow::Result<TokenUnits> {
        anyhow::bail!("this checkpoint chain reads no MONKEY cap")
    }
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
        let state = self.chain.state(View::Finalized).await?;
        ensure!(state.signer == self.signer, "vault signer differs from this enclave; signer recovery is required");
        if let Some((seq, root)) = state.checkpoint {
            let cp = anchor.context("log is shorter than the latest posted checkpoint")?;
            ensure!(cp.seq == seq && cp.root == root, "log differs from the latest posted checkpoint at seq {seq}");
            signature::verify_domain(cp, self.domain)?;
            ensure!(
                cp.signer.parse::<engine_core::types::AccountId>()? == state.signer,
                "posted checkpoint signer mismatch"
            );
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
    pub async fn run_with_recovery(
        self,
        mut rx: mpsc::Receiver<Work>,
        tx: mpsc::Sender<Request>,
        recovery: Vec<Work>,
    ) -> anyhow::Result<()> {
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
                    let result = self.reconcile_known(work, &known).instrument(span.clone()).await;
                    if matches!(result, Ok((Some(_), false))) {
                        let since = pending_since.get_or_insert_with(tokio::time::Instant::now);
                        if since.elapsed() >= self.pending_deposits_critical_after {
                            span.in_scope(|| error!(alert = "collateral_mismatch", "withdrawals paused: collateral reconciliation exceeded its catch-up deadline"));
                        }
                    } else if matches!(result, Ok((_, true))) {
                        pending_since = None;
                    }
                    let (posted, healthy) = match result {
                        Ok(status) => status,
                        Err(error) => {
                            if error.is::<Conflict>() {
                                span.in_scope(|| error!(alert = "checkpoint_conflict", "refusing to continue: {error:#}"));
                                return Err(error);
                            }
                            span.in_scope(|| warn!(alert = "checkpoint_post_failed", "checkpoint remains queued: {error:#}"));
                            (None, false)
                        }
                    };
                    tx.send(Request::CheckpointStatus { posted, healthy }).await.map_err(|_| anyhow::anyhow!("sequencer stopped"))?;
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

    /// Verifies/extends the commitment and compares collateral to the checkpoint's ledger.
    /// Error cases:
    /// - the chain read or post fails, or a signer/root conflicts with the local log
    #[cfg(test)]
    async fn reconcile(&self, work: &Work) -> anyhow::Result<(Option<u64>, bool)> {
        self.reconcile_known(work, &[work]).await
    }

    /// Matches public relays to the authenticated local chain before accepting a covered prefix.
    /// Error cases:
    /// - unavailable/lagging views, unknown progress, or a proved conflicting local commitment
    async fn reconcile_known(&self, work: &Work, known: &[&Work]) -> anyhow::Result<(Option<u64>, bool)> {
        let mut work = work;
        let mut state = self.chain.state(View::Latest).await?;
        tracing::Span::current()
            .record("latest_seq", tracing::field::debug(state.checkpoint.as_ref().map(|(seq, _)| *seq)));
        if let Some((seq, root)) = &state.checkpoint
            && *seq > work.checkpoint.seq
        {
            work = known
                .iter()
                .copied()
                .find(|work| work.checkpoint.seq == *seq)
                .context("vault advanced to a checkpoint not yet received by this worker")?;
            if work.checkpoint.root != *root {
                return Err(Conflict("conflicting publicly relayed checkpoint root").into());
            }
        }
        let cp = &work.checkpoint;
        signature::verify_domain(cp, self.domain)?;
        if state.signer != self.signer || cp.signer.parse::<engine_core::types::AccountId>()? != self.signer {
            return Err(Conflict("checkpoint signer differs from the authorized vault signer").into());
        }
        match &state.checkpoint {
            Some((seq, root)) if *seq == cp.seq => {
                if *root != cp.root {
                    return Err(Conflict("conflicting posted checkpoint root").into());
                }
            }
            Some((seq, _)) if *seq > cp.seq => {
                return Err(Conflict("vault advanced beyond the local checkpoint").into());
            }
            _ => {
                let parent = cp.parent.context("on-chain checkpoints require signed parent metadata")?;
                let expected = state
                    .checkpoint
                    .as_ref()
                    .map(|(seq, root)| -> anyhow::Result<_> {
                        Ok(crate::log::CheckpointParent {
                            has_checkpoint: true,
                            seq: *seq,
                            root: root.parse::<engine_core::eip712::B256>()?,
                        })
                    })
                    .transpose()?
                    .unwrap_or_default();
                if parent != expected {
                    ensure!(
                        !(parent.has_checkpoint && (!expected.has_checkpoint || expected.seq < parent.seq)),
                        "checkpoint view is behind the signed predecessor; awaiting backend catch-up"
                    );
                    return Err(Conflict("signed parent differs from the latest vault checkpoint").into());
                }
                self.chain.post(cp).await?;
                // A submitted transaction is not a commitment. Verify inclusion before release.
                state = self.chain.state(View::Latest).await?;
                ensure!(
                    state.checkpoint == Some((cp.seq, cp.root.clone())) && state.signer == self.signer,
                    "checkpoint transaction is not included in the authorized vault"
                );
            }
        }
        // Latest inclusion prevents duplicate submission, but is revocable by a shallow reorg.
        tracing::Span::current()
            .record("latest_seq", tracing::field::debug(state.checkpoint.as_ref().map(|(seq, _)| *seq)));
        let included_height = state.height;
        let state = self.chain.state(View::Finalized).await?;
        tracing::Span::current()
            .record("finalized_seq", tracing::field::debug(state.checkpoint.as_ref().map(|(seq, _)| *seq)));
        ensure!(included_height >= state.height, "latest checkpoint view is behind the finalized view");
        if state.signer != self.signer {
            return Err(Conflict("finalized vault signer differs from this enclave").into());
        }
        let Some((seq, root)) = &state.checkpoint else { return Ok((None, false)) };
        if *seq < known.iter().map(|work| work.checkpoint.seq).min().unwrap_or(cp.seq) {
            return Ok((None, false));
        }
        let finalized = known
            .iter()
            .copied()
            .find(|work| work.checkpoint.seq == *seq)
            .context("finalized checkpoint has not yet reached this worker")?;
        signature::verify_domain(&finalized.checkpoint, self.domain)?;
        if finalized.checkpoint.root != *root {
            return Err(Conflict("finalized checkpoint conflicts with local log").into());
        }
        let work = finalized;
        let cp = &work.checkpoint;
        if work.coverage == DepositCoverage::AwaitingCanonicalScan {
            warn!(alert = "collateral_pending_deposits", "withdrawals await sequenced canonical receipt coverage");
            return Ok((Some(cp.seq), false));
        }
        let healthy = state.balance == work.liabilities && state.deposits == work.deposits;
        if healthy {
            info!(alert_resolved = "collateral_mismatch", "vault collateral reconciles");
            info!(alert_resolved = "checkpoint_post_failed", "checkpoint is posted");
            info!(alert_resolved = "collateral_pending_deposits", "vault deposits are credited");
        } else if state.deposits > work.deposits && state.balance >= work.liabilities {
            warn!(
                alert = "collateral_pending_deposits",
                "withdrawals await the checkpoint covering final deposit credits"
            );
        } else {
            error!(
                alert = "collateral_mismatch",
                "withdrawals paused: vault collateral or deposit count differs from ledger"
            );
        }
        Ok((Some(cp.seq), healthy))
    }
}

#[cfg(test)]
mod emissions_scenario;
#[cfg(test)]
mod scenario;
#[cfg(test)]
mod tests;
