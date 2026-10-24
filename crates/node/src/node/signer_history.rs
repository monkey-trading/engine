//! Historical signer identities are accepted only from the vault's finalized root history.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::ensure;

use crate::checkpoints::{APPROVAL_PAGE_SIZE, CheckpointChain, SignerHistory, setup::Setup};
use crate::log::{LogEntry, SegmentedLog};

/// Loads immutable accepted identities without opening the log for append or repairing history.
/// Error cases:
/// - the log cannot be read, custody is unavailable, or an accepted identity differs from the log
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
    let mut history = SignerHistory::default();
    for page in candidates.chunks(APPROVAL_PAGE_SIZE) {
        let sequences = page.iter().map(|checkpoint| checkpoint.seq).collect::<Vec<_>>();
        let observed = setup.poster.chain.accepted_checkpoints(&setup.observation, &sequences).await?;
        ensure!(observed.len() == page.len(), "vault lacks accepted checkpoint signer history");
        for (authority, checkpoint) in observed.into_iter().zip(page) {
            authority.verify(checkpoint)?;
            history.checkpoints.push(authority);
        }
    }
    history.authenticate_epochs(&setup.poster.chain, &setup.observation, epochs).await?;
    Ok(history)
}
