//! Chain access the checkpoint worker reads and posts through.

use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::emissions::TokenUnits;

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

    /// The release the vault names as holding the enclave's keys, and its generation, at the already
    /// pinned finalized block.
    /// Error cases:
    /// - the observation is not finalized and pinned, the read fails, or the chain reads no releases
    async fn current_release(&self, at: &Observation) -> anyhow::Result<CurrentRelease> {
        let _ = at;
        anyhow::bail!("this checkpoint chain reads no releases")
    }
}
