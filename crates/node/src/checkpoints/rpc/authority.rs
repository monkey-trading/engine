//! Retired signer reservations require independently accepted canonical checkpoint identities.

use anyhow::ensure;
use engine_core::types::AccountId;
use tracing::warn;

use crate::checkpoints::rpc::CheckpointRpc;
use crate::checkpoints::{CheckpointChain, Observation};

/// A checkpoint the vault had not accepted at the finalized block read: past its anchor, or never.
#[derive(Debug)]
pub(super) struct NotAccepted(pub(super) &'static str);

impl std::fmt::Display for NotAccepted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for NotAccepted {}

impl CheckpointRpc {
    /// Authenticates the journal's work by any key but this start's own signer against the vault's
    /// record at `at`, a finalized observation, before startup consumes its nonce reservation. With
    /// `retire`, a promoted start's, work by another key that `at` does not show accepted is retired:
    /// the promotion moved the signer, so it can never be accepted, and the relayer's nonce is
    /// checked free before the next reservation.
    /// Error cases:
    /// - retired work is unaccepted (without `retire`), or storage is malformed
    pub(crate) async fn authenticate_reservation(&self, at: &Observation, retire: bool) -> anyhow::Result<()> {
        let store = self.submissions()?;
        let Some(reservation) = store.read_untrusted()? else { return Ok(()) };
        for checkpoint in std::iter::once(&reservation.checkpoint).chain(reservation.reused_through.iter()) {
            if checkpoint.signer.parse::<AccountId>()? != store.signer() {
                let accepted = match self.accepted_checkpoints(at, &[checkpoint.seq]).await {
                    Err(e) if retire && e.downcast_ref::<NotAccepted>().is_some() => {
                        warn!(seq = checkpoint.seq, "retiring an outgoing key's checkpoint reservation: {e:#}");
                        return store.retire();
                    }
                    accepted => accepted?,
                };
                ensure!(accepted.len() == 1, "reservation historical authority is unavailable");
                accepted[0].verify(checkpoint)?;
                store.authorize(accepted[0].clone())?;
            }
        }
        store.read()?;
        Ok(())
    }
}
