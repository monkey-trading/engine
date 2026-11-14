//! Chain access the checkpoint worker reads and posts through.

use alloy::primitives::U256;
use anyhow::{Context, ensure};
use engine_core::emissions::TokenUnits;
use engine_core::types::{AccountId, Millis};
use engine_core::{Engine, Fixed};

use crate::checkpoints::{
    APPROVAL_PAGE_SIZE, AcceptedCheckpoint, ApprovalId, ApprovalStatus, LifecycleEvidence, Observation, VaultState,
    View,
};
use crate::log::Checkpoint;

/// The release the vault names as holding the enclave's keys, and the generation it named it in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CurrentRelease {
    /// `keccak256` of the launch measurement and the config hash; zero before governance names one.
    pub id: [u8; 32],
    /// Raised by every change of the current release.
    pub generation: u64,
}

/// The operator an election replaced and the handover data its candidacy registered, as the vault's
/// registry recorded when the election was made: the elected successor reads this to find the
/// incumbent's handover endpoint and archive hostname.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HandoverSource {
    pub candidate: engine_core::types::AccountId,
    pub release: [u8; 32],
    pub api_hostname: String,
    pub handover_endpoint: String,
}

/// The daily turn a schedule the vault's registry names had drawn as in effect at a time, as a
/// finalized block reads it: the member, the release its candidacy holds at that block, the generation
/// the schedule fixed for the turn, and the member the turn takes over from, whose handover endpoint
/// the successor connects to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScheduledTurn {
    pub turn: u64,
    /// When the turn starts, in milliseconds.
    pub boundary: Millis,
    pub candidate: AccountId,
    pub admin_key: AccountId,
    pub release: [u8; 32],
    pub generation: u64,
    /// The rotation set the turn was drawn from.
    pub set: u32,
    /// The operator the turn takes over from, unless it is the member itself.
    pub source: Option<HandoverSource>,
}

/// The operator the vault's registry names in effect at a finalized block while rotation is off,
/// when only the timelock's `installOperator` seats another, and when it was seated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RegistryOperator {
    pub admin_key: AccountId,
    /// The block time the registry seated it, in milliseconds.
    pub since: Millis,
}

impl RegistryOperator {
    /// This operator's key, when `engine`'s log does not yet hand over to it as the registry has it
    /// seated and its staking scan has not passed the seating: the scan's frontier time, before which
    /// every registry event has been mirrored, is no later than the seating's. The log does not when
    /// it records another key, or records this one with a signed rotation to another pending, which
    /// the seating then supersedes. The log may already hold the installation, whose handover then
    /// waits out its notice; an earlier installation of the same key tells nothing, since the
    /// timelock may install a key again. A log whose scan records no frontier time cannot tell, and
    /// reads as scanned.
    pub(crate) fn unscanned(self, engine: &Engine) -> Option<AccountId> {
        let frontier = engine.staking().and_then(|staking| staking.completed_through).and_then(|through| through.time);
        let recorded = engine.operator().is_some_and(|operator| operator.address == self.admin_key);
        let superseded = engine.operator_bounds().rotation.is_some_and(|rotation| rotation.address != self.admin_key);
        (frontier.is_some_and(|time| self.since >= time) && (!recorded || superseded)).then_some(self.admin_key)
    }
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

    /// Whether a claim can prove against the vault's checkpoint `seq` at `at`'s block: one recorded
    /// in the signer epoch in force, whose signer is not retired. An adapter that reads no signer
    /// history has only the current epoch's checkpoints to answer for.
    /// Error cases:
    /// - the checkpoint's epoch cannot be read at that block
    async fn claims_open(&self, at: &Observation, seq: u64) -> anyhow::Result<bool> {
        let _ = seq;
        Ok(!at.state.signer_retired())
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

    /// The release the vault names as holding the enclave's keys, and its generation, at the already
    /// pinned finalized block.
    /// Error cases:
    /// - the observation is not finalized and pinned, the read fails, or the chain reads no releases
    async fn current_release(&self, at: &Observation) -> anyhow::Result<CurrentRelease> {
        let _ = at;
        anyhow::bail!("this checkpoint chain reads no releases")
    }

    /// Whether the vault's exit against the last posted root has started, after which no checkpoint
    /// can be posted. Error cases: the chain cannot be read; a chain with no exit reads `false`.
    async fn exit_started(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    /// The relayer's balance in wei at a block the custody providers agree is final, or `None` when
    /// this chain does not watch it.
    /// Error cases:
    /// - the custody providers have no quorum on it
    async fn relayer_balance(&self) -> anyhow::Result<Option<U256>> {
        Ok(None)
    }

    /// Whether this chain can read the vault's folded release; false for stand-in chains.
    fn reads_release_fold(&self) -> bool {
        false
    }

    /// The release the vault's generation has folded, at the already pinned finalized observation.
    /// Error cases: the observation is not finalized and pinned, the read fails, or the chain
    /// reads no releases
    async fn synced_release(&self, at: &Observation) -> anyhow::Result<[u8; 32]> {
        let _ = at;
        anyhow::bail!("this checkpoint chain reads no release generation")
    }

    /// Whether the operator registry the vault follows has rotation on, at the latest block. Error
    /// cases: the chain cannot be read; a chain with no registry reads `false`.
    async fn rotation_enabled(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    /// Whether the registry has rotation on at the already pinned finalized observation, the state the
    /// vault's current release was read at. Error cases: the observation is not finalized and pinned,
    /// or the chain cannot be read; a chain with no registry reads `false`.
    async fn rotation_enabled_at(&self, at: &Observation) -> anyhow::Result<bool> {
        let _ = at;
        Ok(false)
    }

    /// The handover source the vault's registry recorded when the election in effect was made: the
    /// operator it replaced and the handover endpoint its candidacy registered, which the elected
    /// successor connects to for the keys. Error cases: the chain cannot be read, or it reads no
    /// registry.
    async fn handover_source(&self) -> anyhow::Result<HandoverSource> {
        anyhow::bail!("this checkpoint chain reads no registry")
    }

    /// The API hostname the candidate the registry names as the operator in effect registered.
    /// Error cases: the chain cannot be read, or it reads no registry.
    async fn operator_hostname(&self) -> anyhow::Result<String> {
        anyhow::bail!("this checkpoint chain reads no registry")
    }

    /// `operator_hostname` at the already pinned finalized block.
    /// Error cases: the observation is not finalized and pinned, the chain cannot be read, or it
    /// reads no registry
    async fn operator_hostname_at(&self, at: &Observation) -> anyhow::Result<String> {
        let _ = at;
        anyhow::bail!("this checkpoint chain reads no registry")
    }

    /// The API hostname `candidate` registered, at the already pinned finalized block.
    /// Error cases: as `operator_hostname_at`
    async fn candidate_hostname_at(&self, at: &Observation, candidate: AccountId) -> anyhow::Result<String> {
        let _ = (at, candidate);
        anyhow::bail!("this checkpoint chain reads no registry")
    }

    /// The drawn turn whose boundary is the latest at or before `now_ms`, in the schedule the vault's
    /// registry names, at the already pinned finalized observation; `None` when the registry names no
    /// schedule, no such turn is drawn, or the registry installed an operator after that turn ended,
    /// which ends the turns (`rotationState`, whatever candidate it installed). A chain with no
    /// schedule reads `None`.
    /// Error cases: the observation is not finalized and pinned, or the chain cannot be read
    async fn scheduled_turn(&self, at: &Observation, now_ms: Millis) -> anyhow::Result<Option<ScheduledTurn>> {
        let _ = (at, now_ms);
        Ok(None)
    }
}
