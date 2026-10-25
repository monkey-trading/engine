//! Retired signer reservations require independently accepted canonical checkpoint identities.

use anyhow::ensure;
use engine_core::types::AccountId;

use crate::checkpoints::rpc::CheckpointRpc;
use crate::checkpoints::{CheckpointChain, Observation};

impl CheckpointRpc {
    /// Authenticates old journal identities before startup consumes their nonce reservations.
    /// Error cases:
    /// - retired work is unaccepted, the finalized signer differs, or storage is malformed
    pub(crate) async fn authenticate_reservation(&self, at: &Observation) -> anyhow::Result<()> {
        let Some(reservation) = self.submissions()?.read_untrusted()? else { return Ok(()) };
        for checkpoint in std::iter::once(&reservation.checkpoint).chain(reservation.reused_through.iter()) {
            if checkpoint.signer.parse::<AccountId>()? != at.state.signer {
                let accepted = self.accepted_checkpoints(at, &[checkpoint.seq]).await?;
                ensure!(accepted.len() == 1, "reservation historical authority is unavailable");
                accepted[0].verify(checkpoint)?;
                self.submissions()?.authorize(accepted[0].clone())?;
            }
        }
        self.submissions()?.read()?;
        Ok(())
    }
}
