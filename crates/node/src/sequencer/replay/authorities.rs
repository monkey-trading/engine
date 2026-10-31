//! Historical signer authorities that let replay accept checkpoints a retired key signed.

use anyhow::ensure;

use crate::sequencer::replay::Replay;

impl Replay {
    /// Pins only independently accepted checkpoints within the finalized recovery anchor.
    /// Error cases:
    /// - an authority lies beyond the trusted prefix, or duplicate identities conflict
    pub(in crate::sequencer) fn set_authorities(
        &mut self,
        history: crate::checkpoints::SignerHistory,
    ) -> anyhow::Result<()> {
        self.signer_epochs = history.epochs;
        ensure!(
            self.signer_epochs.iter().all(|(epoch, signer)| *epoch > 0 && signer.0 != [0; 20]),
            "invalid historical signer epoch evidence"
        );
        for authority in history.checkpoints {
            ensure!(
                self.recovery_parent.is_some_and(|parent| parent.has_checkpoint && authority.seq <= parent.seq),
                "historical signer evidence is outside the finalized anchor"
            );
            ensure!(
                self.signer_epochs.get(&authority.signer_epoch) == Some(&authority.signer),
                "checkpoint signer differs from immutable epoch history"
            );
            if let Some(previous) = self.authorities.insert(authority.seq, authority.clone()) {
                // A scheduled and an explicit checkpoint can land at one seq, and a rotation can
                // re-sign the same root: the state commitment is what history pins, so only a
                // different root at one seq conflicts.
                ensure!(previous.root == authority.root, "conflicting historical signer evidence");
            }
        }
        Ok(())
    }
}
