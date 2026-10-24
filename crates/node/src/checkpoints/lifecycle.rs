//! Canonical custody evidence for approvals and historical checkpoint authorities.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use alloy::primitives::B256;
use anyhow::ensure;
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
    pub signer: AccountId,
    pub signer_epoch: u64,
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
