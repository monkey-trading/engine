//! On-chain checkpoint commitments, replay protection, and collateral reconciliation.

pub(crate) mod accounting;
pub(crate) mod emission_budget;
mod history;
pub(crate) use history::accepted_history;
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
use engine_core::emissions::TokenUnits;
use engine_core::staking::UsdcUnits;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{Instrument, debug, error, warn};

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
    /// USDC base units the engine has allocated to stakers across closed epochs; the vault cannot
    /// legitimately have paid more than this. Logs written before distributions existed carry no
    /// value; they had none.
    #[serde(default)]
    pub distributed: UsdcUnits,
    pub authorized: Fixed,
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
        let distributed = engine.staking().map_or(UsdcUnits(0), |staking| staking.distributed);
        Self {
            checkpoint,
            liabilities: accounting.liabilities,
            deposits,
            distributed,
            authorized: accounting.unpaid(),
            coverage,
        }
    }

    /// Whether signing now can produce work with a provable credited prefix.
    pub(crate) fn can_capture(accounting: Accounting, engine: &engine_core::Engine) -> bool {
        !engine.deposit_finality_enabled()
            || engine.custody_deposit_coverage().is_some()
            || Self::empty_prefix(
                accounting.deposits,
                accounting.liabilities,
                accounting.unpaid(),
                engine.staking().map_or(UsdcUnits(0), |staking| staking.distributed),
            )
    }

    /// No allocated custody liability can depend on the identity of a receipt in an empty prefix.
    fn empty_prefix(deposits: u64, liabilities: Fixed, authorized: Fixed, distributed: UsdcUnits) -> bool {
        deposits == 0 && liabilities == Fixed::ZERO && authorized == Fixed::ZERO && distributed == UsdcUnits(0)
    }

    /// An empty ledger has the exact empty credited prefix independently of a custody scan.
    pub(crate) fn has_proven_prefix(&self) -> bool {
        self.coverage != DepositCoverage::AwaitingCanonicalScan
            || Self::empty_prefix(self.deposits, self.liabilities, self.authorized, self.distributed)
    }

    /// Refuses restored signatures that could become an irreconcilable finalized checkpoint.
    /// Error cases:
    /// - a nonempty modern checkpoint lacks its original canonical receipt coverage
    pub(crate) fn validate_recovery_prefix(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.has_proven_prefix(),
            "recovered checkpoint at seq {} has a nonempty unproven custody prefix; \
             later scans cannot authenticate that immutable prefix: preserve this prelaunch log \
             for audit and use a fresh deployment or an explicitly verified operator migration",
            self.checkpoint.seq
        );
        Ok(())
    }

    /// Whether this work carries `accounting`'s liabilities and approvals and, unless sequenced
    /// chain proof counted them, its deposits.
    pub(crate) fn matches_accounting(&self, accounting: Accounting) -> bool {
        self.liabilities == accounting.liabilities
            && self.authorized == accounting.unpaid()
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
    /// USDC the vault's deposits brought in, house deposits and booked surpluses included.
    pub deposited: Fixed,
    /// USDC the vault has paid out in staker distributions; the engine's ledger holds the
    /// liabilities, and this explains what has left the vault since.
    pub distributions_paid: Fixed,
}

/// Latest inclusion is progress; only final custody-policy state authorizes signature release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum View {
    Latest,
    Finalized,
}

/// How the vault's collateral compared with the checkpoint's ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// No finalized ledger comparison is available; preserves the preceding reconciliation verdict.
    Unvalidated,
    /// The finalized checkpoint's nonempty custody prefix has no sequenced canonical receipt
    /// coverage, so its credits cannot be compared with the vault: withdrawals pause until a
    /// newer finalized checkpoint proves coverage. Signing continues, as that checkpoint is the
    /// way out.
    Unproven,
    /// Balance plus paid distributions equals the ledger and the deposit counts match.
    Healthy,
    /// Routine catch-up: the vault counts deposits the ledger has not credited yet, with the
    /// compensated collateral still at or above the ledger.
    Pending,
    /// The ledger and the vault disagree; withdrawals pause and no further checkpoint posts.
    Diverged,
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

    /// Reads nonce use for one bounded page of approvals, in query order, at the block of earlier
    /// finalized evidence, so the worker sweeps further pages without sampling a block for each.
    /// Error cases:
    /// - the page exceeds its bound, the block was orphaned, or the adapter cannot read past blocks
    async fn claims(&self, at: &LifecycleEvidence, approvals: &[ApprovalId]) -> anyhow::Result<Vec<ApprovalStatus>> {
        let _ = (at, approvals);
        anyhow::bail!("adapter cannot read nonce use at an earlier block")
    }

    /// Reads the vault's running deposit total through its first `count` deposits, at the already
    /// sampled finalized block, so a ledger can be compared with exactly the deposits it credited.
    /// Error cases:
    /// - `count` exceeds the observed deposits, or the adapter cannot read a past total
    async fn deposited_through(&self, at: &Observation, count: u64) -> anyhow::Result<Fixed> {
        ensure!(at.view == View::Finalized, "deposit totals require a finalized observation");
        ensure!(count == at.state.deposits, "adapter cannot read a past deposit total");
        Ok(at.state.deposited)
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
    /// How long the first deposit the finalized checkpoint has not credited may stay uncredited.
    pub pending_deposits_critical_after: Duration,
    /// Whether the `checkpoint_withheld` episode for a diverged collateral view is open, so only
    /// the post that ends it logs the resolve.
    pub post_withheld: std::sync::atomic::AtomicBool,
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
    /// - the sequencer disappears, registry is poisoned, or two observations in a row conflict
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
        let mut uncredited = None;
        // A conflict from one observation, not yet repeated by a fresh one.
        let mut unconfirmed = false;
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
                    let result = self.reconcile_lifecycle(work, &known, &page, &mut uncredited).instrument(span.clone()).await;
                    // One read can be torn, its fields answered from either side of a block the endpoint
                    // is still mining, so only a conflict a fresh observation repeats stops the worker.
                    let (posted, health, lifecycle) = match result {
                        Ok((posted, health, lifecycle)) => {
                            unconfirmed = false;
                            (posted, (health != Health::Unvalidated).then_some(health), lifecycle)
                        }
                        Err(error) if unconfirmed && error.is::<Conflict>() => {
                            span.in_scope(|| error!(alert = "checkpoint_conflict", "refusing to continue: {error:#}"));
                            return Err(error);
                        }
                        Err(error) => {
                            unconfirmed |= error.is::<Conflict>();
                            span.in_scope(|| warn!(alert = "checkpoint_post_failed", "checkpoint remains queued: {error:#}"));
                            // A failed post must not hide a finalized pause or retired signer.
                            let halt = if error.is::<Conflict>() { Ok(None) } else { self.observe_halt(&page).instrument(span.clone()).await };
                            let halt = halt.unwrap_or_else(|error| {
                                span.in_scope(|| debug!("vault halt observation failed: {error:#}"));
                                None
                            });
                            (None, None, halt)
                        }
                    };
                    if lifecycle.is_some() {
                        approval_cursor = page.last().map(|approval| (approval.account, approval.nonce));
                    }
                    let evidence = lifecycle.clone();
                    tx.send(Request::CheckpointStatus { posted, health, lifecycle }).await.map_err(|_| anyhow::anyhow!("sequencer stopped"))?;
                    if let Some(evidence) = &evidence {
                        approval_cursor = self
                            .sweep(evidence, &approvals, approval_cursor, posted, health, &tx)
                            .instrument(span.clone())
                            .await?;
                    }
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
mod sweep;
use sweep::approval_page;

#[cfg(test)]
mod emissions_scenario;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod scenario;
#[cfg(test)]
mod staking_scenario;
#[cfg(test)]
mod tests;
