//! The pass over a replay's ranges in log order: it resolves what each range's entries named before the
//! range against the ranges before it, and adds the ranges' results up into an audit's verdict. It applies
//! nothing and reads nothing from a snapshot.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, ensure};
use engine_core::eip712::B256;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};

use crate::sequencer::replay::Replayed;
use crate::sequencer::replay::ranges::{RangeReplayed, Refs, Trust};

/// What an audit of a log verified, as `engine replay` reports it.
pub struct Audited {
    pub commands: u64,
    pub checkpoints: u64,
    /// The state the log ends in.
    pub engine: Engine,
    /// Each protocol version the log activated, with the seq it took effect at.
    pub activations: Vec<(ProtocolVersion, u64)>,
    /// The named checkpoint signers.
    pub checkpoint_signers: BTreeSet<AccountId>,
    /// Approved withdrawals the log ends before signing: the last command's and the deferred ones.
    pub pending: usize,
    /// How many ranges the log was replayed in: one for a whole-log replay.
    pub ranges: usize,
}

impl From<Replayed> for Audited {
    /// A whole-log replay's verdict.
    fn from(replayed: Replayed) -> Self {
        Self {
            commands: replayed.commands,
            checkpoints: replayed.checkpoints,
            engine: replayed.engine,
            activations: replayed.activations,
            checkpoint_signers: replayed.checkpoint_signers,
            pending: replayed.unsigned.len() + replayed.deferred.len(),
            ranges: 1,
        }
    }
}

/// The `ranges` ranges' `results`, in log order, added up under `trust`, the results of the ranges after the
/// first that failed left out: the first that failed fails the whole, as
/// does an entry of a range that named a checkpoint or a deferred approval no earlier range holds, at that
/// range, before any later range's fault.
/// Error cases:
/// - a range failed, or a range's entry named what no earlier range holds
/// - under custody trust, no range held the vault's finalized anchor
pub(super) fn stitch(
    results: Vec<anyhow::Result<RangeReplayed>>,
    ranges: usize,
    trust: &Trust,
) -> anyhow::Result<Audited> {
    let mut roots: BTreeMap<u64, B256> = BTreeMap::new();
    let mut deferred: Vec<(u64, AccountId, Fixed, u64)> = Vec::new();
    let (mut commands, mut checkpoints, mut anchored) = (0, 0, false);
    let (mut activations, mut checkpoint_signers) = (Vec::new(), BTreeSet::new());
    let mut last = None;
    for (at, result) in results.into_iter().enumerate() {
        let RangeReplayed { replayed, refs, anchored: held } =
            result.with_context(|| format!("replaying range {} of {ranges}", at + 1))?;
        if let Some(refs) = refs {
            resolve(&refs, &roots, &mut deferred).with_context(|| format!("range {} of {ranges}", at + 1))?;
        }
        roots.extend(replayed.checkpoint_roots.iter().map(|(seq, root)| (*seq, *root)));
        deferred.extend(replayed.deferred.iter().copied());
        commands += replayed.commands;
        // Each range after the first applies the checkpoint the one before it ended at, again.
        checkpoints += replayed.checkpoints - u64::from(at > 0);
        activations.extend(replayed.activations.iter().copied());
        checkpoint_signers.extend(replayed.checkpoint_signers.iter().copied());
        anchored |= held;
        last = Some(replayed);
    }
    let last = last.context("a log is replayed in at least one range")?;
    if let Trust::Custody { parent, .. } = trust {
        ensure!(!parent.has_checkpoint || anchored, "public log is shorter than or omits the finalized vault anchor");
    }
    Ok(Audited {
        commands,
        checkpoints,
        pending: last.unsigned.len() + deferred.len(),
        engine: last.engine,
        activations,
        checkpoint_signers,
        ranges,
    })
}

/// Resolves what a range's entries named before it, `refs`, against `roots`, the checkpoints of the ranges
/// before it, and `deferred`, the deferred approvals they left unreleased, taking each released one out.
/// Error cases:
/// - a typed payout names a checkpoint no earlier range holds, or holds with another root
/// - a withdrawal entry releases a deferred approval no earlier range left unreleased
pub(super) fn resolve(
    refs: &Refs,
    roots: &BTreeMap<u64, B256>,
    deferred: &mut Vec<(u64, AccountId, Fixed, u64)>,
) -> anyhow::Result<()> {
    for (seq, root) in &refs.payouts {
        ensure!(roots.get(seq) == Some(root), "typed payout checkpoint is absent from the preceding log");
    }
    for released in &refs.deferred {
        let at = deferred.iter().position(|held| held == released).context("deferred withdrawal was not approved")?;
        deferred.remove(at);
    }
    Ok(())
}
