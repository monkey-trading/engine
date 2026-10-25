//! Historical signer identities come from the log's signed-parent chain to the vault's finalized
//! anchor and from the vault's signer epochs.

use std::collections::BTreeSet;
use std::path::Path;

use crate::checkpoints::{SignerHistory, accepted_history, setup::Setup};
use crate::log::{LogEntry, SegmentedLog};

/// Loads immutable accepted identities without opening the log for append or repairing history.
/// Error cases:
/// - the log cannot be read, custody is unavailable, or the chain breaks or misses the anchor
pub(super) async fn load(setup: Option<&Setup>, dir: &Path) -> anyhow::Result<SignerHistory> {
    let Some(setup) = setup else { return Ok(SignerHistory::default()) };
    let mut candidates = Vec::new();
    let mut epochs = BTreeSet::new();
    SegmentedLog::open(dir, |entry| {
        if let LogEntry::Withdrawal(withdrawal) = &entry
            && let Some(auth) = &withdrawal.authorization
        {
            epochs.insert(auth.signer_epoch);
        }
        if let LogEntry::Checkpoint(checkpoint) = entry
            && setup.parent.has_checkpoint
            && checkpoint.seq <= setup.parent.seq
            && checkpoint.parent.is_some()
        {
            candidates.push(checkpoint);
        }
        Ok(())
    })?;
    accepted_history(&setup.poster.chain, &setup.observation, setup.poster.domain, setup.parent, &candidates, epochs)
        .await
}
