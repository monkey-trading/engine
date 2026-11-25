//! Historical signer identities come from the log's signed-parent chain to the vault's finalized
//! anchor and from the vault's signer epochs: from the log's start, or, for a start from a daily
//! snapshot, from the snapshot's checkpoint, with no read of the log before it.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::ensure;
use engine_core::types::AccountId;

use crate::checkpoints::{SignerHistory, accepted_history, setup::Setup};
use crate::log::{Checkpoint, CheckpointParent, LogEntry, Offset, SegmentedLog};

/// Where a start from a daily snapshot goes on: its checkpoint's offset, the checkpoint, and the signers of every
/// checkpoint through it, as the snapshot carries them.
pub(super) type Start = (Offset, Checkpoint, BTreeSet<AccountId>);

/// Loads immutable accepted identities without opening the log for append or repairing history: from
/// `start`, a daily snapshot the start takes, reading only the log after it, or from the log's start. A typed payout after the snapshot that names a checkpoint before it takes that
/// checkpoint's authority from the vault.
/// Error cases:
/// - the log cannot be read, custody is unavailable, or the chain breaks or misses the anchor
/// - the log holds another entry at the snapshot's offset than its checkpoint
pub(super) async fn load(setup: Option<&Setup>, dir: &Path, start: Option<Start>) -> anyhow::Result<SignerHistory> {
    let Some(setup) = setup else { return Ok(SignerHistory::default()) };
    let (mut candidates, mut epochs, mut earlier) = (Vec::new(), BTreeSet::new(), BTreeSet::new());
    let from_seq = start.as_ref().map(|(_, checkpoint, _)| checkpoint.seq);
    let mut at_start = start.is_none();
    let mut visit = |entry: LogEntry| {
        if !at_start {
            at_start = true;
            let (offset, checkpoint, _) = start.as_ref().expect("a read from a snapshot has one");
            ensure!(
                matches!(&entry, LogEntry::Checkpoint(held) if held == checkpoint),
                "the log holds another entry at {offset} than the daily snapshot's checkpoint"
            );
            return Ok(());
        }
        if let LogEntry::Withdrawal(withdrawal) = &entry
            && let Some(auth) = &withdrawal.authorization
        {
            epochs.insert(auth.signer_epoch);
            if from_seq.is_some_and(|from| auth.checkpoint_seq <= from) {
                earlier.insert(auth.checkpoint_seq);
            }
        }
        if let LogEntry::Checkpoint(checkpoint) = entry
            && setup.parent.has_checkpoint
            && checkpoint.seq <= setup.parent.seq
            && checkpoint.parent.is_some()
        {
            candidates.push(checkpoint);
        }
        Ok(())
    };
    let from = match &start {
        Some((offset, checkpoint, _)) => {
            SegmentedLog::open_from(dir, *offset, &mut visit)?;
            match checkpoint.parent {
                Some(_) => {
                    CheckpointParent { has_checkpoint: true, seq: checkpoint.seq, root: checkpoint.root.parse()? }
                }
                None => CheckpointParent::default(),
            }
        }
        None => {
            SegmentedLog::open(dir, &mut visit)?;
            CheckpointParent::default()
        }
    };
    let (chain, at, domain) = (&setup.poster.chain, &setup.observation, setup.poster.domain);
    let prior = start.as_ref().map(|(_, _, prior)| prior.clone()).unwrap_or_default();
    accepted_history(chain, at, domain, (setup.parent, from, &prior), &candidates, (epochs, earlier)).await
}
