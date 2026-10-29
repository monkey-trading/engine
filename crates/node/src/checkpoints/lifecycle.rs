//! Canonical custody evidence for approvals and historical checkpoint authorities.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use alloy::primitives::B256;
use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::types::AccountId;

use crate::checkpoints::{VaultState, View};
use crate::log::Checkpoint;

pub(crate) const APPROVAL_PAGE_SIZE: usize = 64;

/// An immutable engine approval, registered again from replay after every restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ApprovalId {
    pub account: AccountId,
    pub nonce: u64,
    pub approved_seq: u64,
}

pub(crate) type ApprovalRegistry = Arc<Mutex<BTreeMap<(AccountId, u64), ApprovalId>>>;

/// A nonce-use answer for precisely the approval that was queried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalStatus {
    pub approval: ApprovalId,
    pub claimed: bool,
}

/// All fields come from the same canonical block as the accompanying vault state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleEvidence {
    pub height: u64,
    pub block_hash: B256,
    /// Zero once the vault's guardian retired the signer, until the timelock installs one.
    pub signer: AccountId,
    pub signer_epoch: u64,
    /// The vault refuses withdrawal claims: its timelock or its guardian paused them.
    pub withdrawals_paused: bool,
    pub total_withdrawn: Fixed,
    pub checkpoint_seq: Option<u64>,
    pub checkpoint_root: Option<String>,
    pub approvals: Vec<ApprovalStatus>,
}

/// Existing chain adapters can omit lifecycle support without fabricating unpaid evidence.
#[derive(Clone, Debug)]
pub(crate) struct Observation {
    pub view: View,
    pub state: VaultState,
    pub lifecycle: Option<LifecycleEvidence>,
}

/// Immutable contract metadata authenticates the signer of an accepted historical root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AcceptedCheckpoint {
    pub seq: u64,
    pub root: String,
    pub signer: AccountId,
    pub signer_epoch: u64,
}

/// Independently pinned checkpoint identities and immutable payout signer epochs.
#[derive(Debug, Default)]
pub(crate) struct SignerHistory {
    pub checkpoints: Vec<AcceptedCheckpoint>,
    pub epochs: BTreeMap<u64, AccountId>,
}

impl SignerHistory {
    /// Reads only requested epochs, accepted-root epochs and the current epoch at one pinned block.
    /// Error cases:
    /// - any signer is absent, future, inconsistent, or cannot be authenticated by custody
    pub(crate) async fn authenticate_epochs(
        &mut self,
        chain: &impl crate::checkpoints::CheckpointChain,
        at: &Observation,
        mut epochs: BTreeSet<u64>,
    ) -> anyhow::Result<()> {
        let evidence = at.lifecycle.as_ref().context("signer history requires canonical lifecycle evidence")?;
        epochs.extend(self.checkpoints.iter().map(|checkpoint| checkpoint.signer_epoch));
        epochs.insert(evidence.signer_epoch);
        let epochs: Vec<_> = epochs.into_iter().collect();
        for page in epochs.chunks(APPROVAL_PAGE_SIZE) {
            ensure!(
                page.iter().all(|epoch| *epoch > 0 && *epoch <= evidence.signer_epoch),
                "signer epoch is outside finalized history"
            );
            let signers = chain.epoch_signers(at, page).await?;
            ensure!(signers.len() == page.len(), "vault lacks requested signer epoch history");
            for (&epoch, signer) in page.iter().zip(signers) {
                ensure!(
                    signer.0 != [0; 20] && (epoch != evidence.signer_epoch || signer == evidence.signer),
                    "signer epoch history differs from finalized custody"
                );
                self.epochs.insert(epoch, signer);
            }
        }
        Ok(())
    }
}

impl VaultState {
    /// Whether the vault's guardian retired its signer and the timelock has not installed one.
    pub(crate) fn signer_retired(&self) -> bool {
        self.signer.0 == [0; 20]
    }
}

impl LifecycleEvidence {
    /// Whether the vault's guardian retired its signer and the timelock has not installed one.
    pub fn signer_retired(&self) -> bool {
        self.signer.0 == [0; 20]
    }

    /// Whether the vault refuses every withdrawal claim: they are paused, or it has no signer.
    pub fn halts_withdrawals(&self) -> bool {
        self.withdrawals_paused || self.signer_retired()
    }
}

impl AcceptedCheckpoint {
    /// Requires exact identity, independent of the log's self-declared signer.
    /// Error cases:
    /// - the accepted sequence, root, signer, or epoch does not authenticate this checkpoint
    pub(crate) fn verify(&self, checkpoint: &Checkpoint) -> anyhow::Result<()> {
        ensure!(
            self.seq == checkpoint.seq
                && self.root == checkpoint.root
                && self.signer == checkpoint.signer.parse::<AccountId>()?
                && self.signer.0 != [0; 20]
                && self.signer_epoch > 0,
            "checkpoint differs from accepted historical authority"
        );
        Ok(())
    }
}

impl Observation {
    /// Rejects inconsistent adapter evidence before it can affect signing or accounting.
    /// Error cases:
    /// - lifecycle metadata is not for this vault state, or the page differs from the query
    pub(crate) fn validate(&self, approvals: &[ApprovalId]) -> anyhow::Result<()> {
        if let Some(evidence) = &self.lifecycle {
            ensure!(
                evidence.height == self.state.height
                    && evidence.signer == self.state.signer
                    && evidence.signer_epoch > 0
                    && evidence.checkpoint_seq == self.state.checkpoint.as_ref().map(|value| value.0)
                    && evidence.checkpoint_root == self.state.checkpoint.as_ref().map(|value| value.1.clone())
                    && evidence.approvals.len() == approvals.len()
                    && evidence.approvals.iter().zip(approvals).all(|(status, approval)| status.approval == *approval),
                "lifecycle evidence differs from the canonical vault observation or requested approvals"
            );
        }
        Ok(())
    }
}
