//! The vault's word on a daily snapshot's checkpoint, as a start that reads the vault takes it: the root and signer
//! of the checkpoint the vault accepted at a snapshot's seq, each read alone, so a seq the vault accepted none at
//! leaves the others answered.

use std::collections::BTreeMap;
use std::path::Path;

use engine_core::eip712::B256;
use engine_core::types::AccountId;

use crate::checkpoints::{CheckpointChain, Observation};
use crate::daily_snapshots::AcceptedRoots;

/// The roots and signers the vault accepted, read at the finalized observation `at`.
pub(crate) struct VaultRoots<'a, C> {
    pub(crate) vault: &'a C,
    pub(crate) at: &'a Observation,
}

impl<C: CheckpointChain> AcceptedRoots for VaultRoots<'_, C> {
    /// The root the vault accepted at `seq`; none where it accepted none there, or the read fails.
    async fn root_at(&self, seq: u64) -> Option<B256> {
        let accepted = self.vault.accepted_checkpoints(self.at, &[seq]).await.ok()?;
        accepted.into_iter().find(|checkpoint| checkpoint.seq == seq)?.root.parse().ok()
    }

    /// The signer of the checkpoint the vault accepted at `seq`, or, where it accepted none, of the one it finalized,
    /// which a snapshot before it rests on (`anchor_signer`).
    async fn signer_at(&self, seq: u64) -> Option<AccountId> {
        match accepted_signer(self.vault, self.at, seq).await {
            Some(signer) => Some(signer),
            None => Some(anchor_signer(self.vault, self.at).await),
        }
    }
}

/// The signer of the checkpoint the vault, read at `at`, accepted at `seq`; none where it accepted none there, or
/// the read fails.
pub(crate) async fn accepted_signer(vault: &impl CheckpointChain, at: &Observation, seq: u64) -> Option<AccountId> {
    let accepted = vault.accepted_checkpoints(at, &[seq]).await.ok()?;
    accepted.into_iter().find(|checkpoint| checkpoint.seq == seq).map(|checkpoint| checkpoint.signer)
}

/// The signer of the checkpoint the vault, read at `at`, finalized, or its signer at `at` where it finalized none or
/// the read fails.
pub(crate) async fn anchor_signer(vault: &impl CheckpointChain, at: &Observation) -> AccountId {
    let anchor = at.state.checkpoint.as_ref().map(|(anchor, _)| *anchor);
    match anchor {
        Some(anchor) => accepted_signer(vault, at, anchor).await.unwrap_or(at.state.signer),
        None => at.state.signer,
    }
}

/// The signer of the checkpoint the vault, read at `at`, accepted at each seq a start from the daily snapshots in
/// `snapshots` may go on from at or before `finalized` (`start::candidate_seqs`); a seq it accepted none at, or whose
/// read fails, is left out.
pub(crate) async fn accepted_signers(
    vault: &impl CheckpointChain,
    at: &Observation,
    (snapshots, finalized): (&Path, u64),
) -> BTreeMap<u64, AccountId> {
    let mut accepted = BTreeMap::new();
    for seq in super::start::candidate_seqs(snapshots, finalized) {
        if let Some(signer) = accepted_signer(vault, at, seq).await {
            accepted.insert(seq, signer);
        }
    }
    accepted
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use engine_core::Fixed;

    use super::*;
    use crate::checkpoints::{AcceptedCheckpoint, VaultState, View};

    /// A vault that accepted a checkpoint, signed by the key it names, at each seq it holds, and that refuses a read
    /// naming any other, as the vault's adapter refuses a page naming a seq it accepted none at.
    struct Vault(BTreeMap<u64, AccountId>);

    impl CheckpointChain for Vault {
        async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
            unreachable!("the snapshot's signer is read at the observation given")
        }

        async fn post(&self, _checkpoint: &crate::log::Checkpoint) -> anyhow::Result<()> {
            unreachable!("a start posts nothing")
        }

        async fn accepted_checkpoints(
            &self,
            _at: &Observation,
            sequences: &[u64],
        ) -> anyhow::Result<Vec<AcceptedCheckpoint>> {
            let accepted = |seq: &u64| {
                let signer = *self.0.get(seq).context("checkpoint was not accepted by the vault")?;
                anyhow::Ok(AcceptedCheckpoint { seq: *seq, root: String::new(), signer, signer_epoch: 1 })
            };
            sequences.iter().map(accepted).collect()
        }
    }

    /// A snapshot's checkpoint must be by the key that signed the checkpoint the vault accepted at its seq, an earlier
    /// signer's after a signer change too; at a seq the vault accepted none at, by the finalized checkpoint's signer,
    /// which it rests on, though the read of its own seq is refused; and by the vault's signer when neither reads.
    #[tokio::test]
    async fn a_snapshot_is_by_the_signer_the_vault_accepted_at_its_seq_or_at_its_anchor() {
        let (earlier, finalized, now) = (AccountId([1; 20]), AccountId([2; 20]), AccountId([3; 20]));
        let zero = Fixed::ZERO;
        let checkpoint = Some((20, String::new()));
        let state = VaultState {
            height: 0,
            signer: now,
            checkpoint,
            balance: zero,
            deposits: 0,
            deposited: zero,
            distributions_paid: zero,
        };
        let at = Observation { view: View::Finalized, state, lifecycle: None };
        let vault = Vault([(10, earlier), (20, finalized)].into());
        let roots = VaultRoots { vault: &vault, at: &at };
        assert_eq!(roots.signer_at(10).await, Some(earlier), "the signer the vault accepted at the seq");
        assert_eq!(roots.signer_at(15).await, Some(finalized), "a seq it accepted none at rests on its anchor");
        let unread = Vault(BTreeMap::new());
        let roots = VaultRoots { vault: &unread, at: &at };
        assert_eq!(roots.signer_at(15).await, Some(now), "the vault's signer once neither reads");
    }
}
