//! Concrete replay facts at a signed checkpoint. Digests never substitute for custody work.

use std::collections::BTreeSet;

use anyhow::{Context as _, ensure};
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};
use serde::{Deserialize, Serialize};

use crate::checkpoints::Work;
use crate::checkpoints::accounting::Accounting;
use crate::checkpoints::signature;
use crate::log::{Checkpoint, CheckpointDomain, CheckpointParent};

/// The independently finalized starting point of the concrete outstanding checkpoint chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RecoveryBase {
    Ungated,
    Genesis,
    Checkpoint { checkpoint: Checkpoint },
}

/// Required values authenticated with the engine, including each checkpoint's original accounting.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryContext {
    pub commands: u64,
    pub command_prefix: [u8; 32],
    pub accounting: Accounting,
    pub logs_events: bool,
    pub unsigned: Vec<(AccountId, Fixed, u64)>,
    pub deferred: Vec<(u64, AccountId, Fixed, u64)>,
    pub checkpoint_signers: BTreeSet<AccountId>,
    pub base: RecoveryBase,
    pub work: Vec<Work>,
}

impl RecoveryBase {
    /// The signed chain predecessor, or no checkpoint service for an ungated log.
    pub(crate) fn parent(&self) -> Option<CheckpointParent> {
        match self {
            Self::Ungated => None,
            Self::Genesis => Some(CheckpointParent::default()),
            Self::Checkpoint { checkpoint } => Some(CheckpointParent {
                has_checkpoint: true,
                seq: checkpoint.seq,
                root: checkpoint.root.parse().expect("a recovery base is signature checked before use"),
            }),
        }
    }

    /// Authenticates the base witness in the deployment supplied by the caller.
    /// Error cases:
    /// - the base signature, signer, or deployment is inconsistent
    fn validate(&self, domain: CheckpointDomain, signer: AccountId) -> anyhow::Result<()> {
        if let Self::Checkpoint { checkpoint } = self {
            signature::verify_domain(checkpoint, domain)?;
            ensure!(checkpoint.signer.parse::<AccountId>()? == signer, "snapshot base signer mismatch");
        }
        Ok(())
    }
}

impl RecoveryContext {
    /// Checks required concrete work and its exact signed parent chain before restoring replay.
    /// Error cases:
    /// - context, signatures, deployment, or checkpoint accounting disagree
    pub(crate) fn validate(&self, checkpoint: &Checkpoint, domain: CheckpointDomain) -> anyhow::Result<()> {
        let signer = checkpoint.signer.parse::<AccountId>()?;
        self.base.validate(domain, signer)?;
        ensure!(
            self.commands == checkpoint.seq.checked_add(1).context("snapshot command count overflow")?,
            "snapshot command count mismatch"
        );
        ensure!(self.checkpoint_signers.contains(&signer), "snapshot checkpoint signer is missing");
        ensure!(self.unsigned.is_empty(), "snapshot checkpoint precedes unsigned withdrawals");
        ensure!(self.logs_events, "snapshot does not record the event-logging transition");
        let mut parent = self.base.parent();
        for work in &self.work {
            signature::verify_domain(&work.checkpoint, domain)?;
            ensure!(work.checkpoint.signer.parse::<AccountId>()? == signer, "snapshot work signer mismatch");
            ensure!(parent.is_some() && work.checkpoint.parent == parent, "snapshot work parent mismatch");
            ensure!(work.checkpoint.seq <= checkpoint.seq, "snapshot work extends beyond engine state");
            parent = Some(CheckpointParent {
                has_checkpoint: true,
                seq: work.checkpoint.seq,
                root: work.checkpoint.root.parse()?,
            });
        }
        if checkpoint.parent.is_some() {
            let last = self.work.last().context("snapshot is missing concrete checkpoint work")?;
            ensure!(last.checkpoint == *checkpoint, "snapshot work does not reach its checkpoint");
            ensure!(last.matches_accounting(self.accounting), "snapshot checkpoint accounting mismatch");
        } else {
            ensure!(
                matches!(self.base, RecoveryBase::Ungated) && self.work.is_empty(),
                "ungated snapshot carries a gated checkpoint chain"
            );
            ensure!(self.deferred.is_empty(), "ungated snapshot carries deferred withdrawals");
        }
        ensure!(
            self.deferred.iter().all(|(seq, _, _, _)| *seq <= checkpoint.seq),
            "snapshot approval extends beyond engine state"
        );
        Ok(())
    }

    /// Recomputes the checkpoint's custody work from the engine it was captured with, including
    /// canonical receipt coverage, which accounting alone does not determine.
    /// Error cases:
    /// - the cached work differs from the work the engine gives
    pub(crate) fn validate_engine(&self, checkpoint: &Checkpoint, engine: &Engine) -> anyhow::Result<()> {
        let Some(last) = self.work.last().filter(|_| checkpoint.parent.is_some()) else { return Ok(()) };
        let expected = Work::capture(checkpoint.clone(), self.accounting, engine);
        ensure!(
            (last.liabilities, last.deposits, last.distributed, last.coverage)
                == (expected.liabilities, expected.deposits, expected.distributed, expected.coverage),
            "snapshot checkpoint accounting mismatch"
        );
        Ok(())
    }

    /// Selects the exact outstanding work after a freshly supplied finalized checkpoint.
    /// Error cases:
    /// - the cache omits the requested prefix or contradicts the independently supplied root
    pub(crate) fn after_anchor(&self, parent: CheckpointParent) -> anyhow::Result<Vec<Work>> {
        let base = self.base.parent().context("gated recovery cannot use an ungated cache")?;
        ensure!(
            !base.has_checkpoint || (parent.has_checkpoint && parent.seq >= base.seq),
            "snapshot does not contain work before its finalized base"
        );
        if parent == base {
            return Ok(self.work.clone());
        }
        if parent.has_checkpoint {
            if let Some(work) = self.work.iter().find(|work| work.checkpoint.seq == parent.seq) {
                ensure!(work.checkpoint.root == parent.root.to_string(), "snapshot finalized root mismatch");
            } else {
                ensure!(
                    self.work.last().is_some_and(|work| work.checkpoint.seq < parent.seq),
                    "snapshot is missing the finalized checkpoint witness"
                );
            }
        } else {
            ensure!(!base.has_checkpoint, "snapshot finalized base mismatch");
        }
        Ok(self
            .work
            .iter()
            .filter(|work| !parent.has_checkpoint || work.checkpoint.seq > parent.seq)
            .cloned()
            .collect())
    }
}
