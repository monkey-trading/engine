//! Reservation transitions require actual vault progress, never transaction hashes or nonces alone.

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, U64};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};

use crate::checkpoints::rpc::submission::Reservation;
use crate::checkpoints::rpc::{CheckpointRpc, hasCheckpointAtCall, postCheckpointCall};
use crate::checkpoints::{CheckpointChain, VaultState, View};
use crate::log::{Checkpoint, CheckpointParent};

impl CheckpointRpc {
    /// Relays exact work, retaining ownership across failed sends and public supersession.
    /// Error cases:
    /// - authorization, canonical progress, nonce ownership, durability, or RPC checks fail
    pub(in crate::checkpoints::rpc) async fn post_reserved(&self, cp: &Checkpoint) -> anyhow::Result<()> {
        self.submissions()?.validate_checkpoint(cp)?;
        let _lock = self.submissions()?.lock()?;
        let latest = self.checked_latest(cp).await?;
        if latest.checkpoint == Some((cp.seq, cp.root.clone())) {
            return Ok(());
        }
        self.check_parent(cp, &latest)?;
        let latest_nonce = self.relayer_nonce(BlockNumberOrTag::Latest).await?;
        let mut reservation = match self.submissions()?.read()? {
            Some(previous) if previous.checkpoint == *cp => {
                if latest_nonce > previous.nonce {
                    ensure!(latest_nonce == previous.nonce + 1, "unrelated relayer nonce consumption");
                    match previous.reused_through.as_ref() {
                        Some(obsolete) => {
                            self.check_finalized_parent(cp, obsolete).await?;
                            self.verify_consumer(&previous, obsolete).await?;
                            self.reserve_after_progress(cp, latest_nonce, None).await?
                        }
                        None => self
                            .reserve_after_revert(cp, &previous, latest_nonce)
                            .await
                            .context("reserved nonce is consumed; await canonical checkpoint reconciliation")?,
                    }
                } else {
                    ensure!(latest_nonce == previous.nonce, "relayer nonce is behind its reservation");
                    previous
                }
            }
            Some(previous) => {
                ensure!(cp.seq > previous.checkpoint.seq, "checkpoint differs from its nonce reservation");
                // Neither public latest inclusion nor an account nonce proves obsolescence.
                self.check_finalized_parent(cp, &previous.checkpoint).await?;
                ensure!(latest_nonce >= previous.nonce, "relayer nonce is behind its reservation");
                let reused_through = if latest_nonce == previous.nonce {
                    Some(previous.checkpoint.clone())
                } else {
                    ensure!(latest_nonce == previous.nonce + 1, "unrelated relayer nonce consumption");
                    self.verify_consumer(&previous, &previous.checkpoint).await?;
                    None
                };
                self.reserve_after_progress(cp, latest_nonce, reused_through).await?
            }
            None => {
                self.check_free_nonce(latest_nonce).await?;
                self.submissions()?.reserve(cp.clone(), latest_nonce, None)?
            }
        };
        let tx = TransactionRequest::default()
            .to(Address::from(self.domain.vault.0))
            .from(self.relayer)
            .nonce(reservation.nonce)
            .input(checkpoint_input(cp)?.into());
        let tx = TransactionRequest { chain_id: Some(self.domain.chain_id), ..tx };
        if let Some(hash) = reservation.transaction_hash {
            let known: Option<TransactionRequest> =
                self.deadline(self.client.request("eth_getTransactionByHash", (hash,))).await?;
            if let Some(known) = known {
                ensure!(same_transaction(&known, &tx)?, "reserved transaction hash identifies another transaction");
                return Ok(());
            }
        }
        // A preceding sync failure must not turn a merely visible reservation into a send.
        self.submissions()?.sync()?;
        let hash = self.deadline(self.client.request("eth_sendTransaction", (tx,))).await?;
        reservation.transaction_hash = Some(hash);
        self.submissions()?.store(&reservation)?;
        Ok(())
    }

    /// Verifies the endpoint deployment and independently configured checkpoint authority.
    /// Error cases:
    /// - the endpoint, checkpoint, or actual vault signer differs
    async fn checked_latest(&self, cp: &Checkpoint) -> anyhow::Result<VaultState> {
        let state = self.state(View::Latest).await?;
        ensure!(
            state.signer == cp.signer.parse::<engine_core::types::AccountId>()?,
            "vault signer differs from the submission authority"
        );
        Ok(state)
    }

    /// Requires the exact signed predecessor before relaying a successor.
    /// Error cases:
    /// - canonical progress differs from the signed parent
    fn check_parent(&self, cp: &Checkpoint, state: &VaultState) -> anyhow::Result<()> {
        let expected = state
            .checkpoint
            .as_ref()
            .map(|(seq, root)| -> anyhow::Result<_> {
                Ok(CheckpointParent { has_checkpoint: true, seq: *seq, root: root.parse()? })
            })
            .transpose()?
            .unwrap_or_default();
        ensure!(cp.parent == Some(expected), "signed parent differs from canonical vault progress");
        Ok(())
    }

    /// Checks actual finalized coverage; journal provenance never substitutes for this read.
    /// Error cases:
    /// - finality lags, the authority/parent differs, or coverage conflicts
    async fn check_finalized_parent(&self, cp: &Checkpoint, obsolete: &Checkpoint) -> anyhow::Result<()> {
        let state = self.state(View::Finalized).await?;
        ensure!(
            state.signer == cp.signer.parse::<engine_core::types::AccountId>()?,
            "finalized vault signer differs from submission authority"
        );
        self.check_parent(cp, &state)?;
        let (seq, root) = state.checkpoint.context("previous checkpoint submission is still pending finality")?;
        ensure!(
            seq >= obsolete.seq && (seq != obsolete.seq || root == obsolete.root),
            "previous checkpoint submission is still pending finality"
        );
        Ok(())
    }

    /// Refuses allocation around unidentified pending transactions or a moving account nonce.
    /// Error cases:
    /// - nonce reads changed, pending work exists, or the account is exhausted
    async fn check_free_nonce(&self, nonce: u64) -> anyhow::Result<()> {
        let latest = self.relayer_nonce(BlockNumberOrTag::Latest).await?;
        let pending = self.relayer_nonce(BlockNumberOrTag::Pending).await?;
        ensure!(latest == nonce, "relayer nonce changed during reconciliation");
        ensure!(pending == nonce, "relayer has unreserved pending transactions; use a dedicated relayer");
        ensure!(nonce < u64::MAX, "relayer nonce is exhausted");
        Ok(())
    }

    /// Reads the relayer nonce at the requested progress tag.
    /// Error cases:
    /// - the RPC fails, times out, or returns a malformed quantity
    async fn relayer_nonce(&self, tag: BlockNumberOrTag) -> anyhow::Result<u64> {
        let nonce: U64 = self.deadline(self.client.request("eth_getTransactionCount", (self.relayer, tag))).await?;
        Ok(nonce.to())
    }

    /// Rechecks canonical parent and account freedom before atomically moving nonce ownership.
    /// Error cases:
    /// - progress changed, a nonce is occupied, or durable storage fails
    async fn reserve_after_progress(
        &self,
        cp: &Checkpoint,
        nonce: u64,
        reused_through: Option<Checkpoint>,
    ) -> anyhow::Result<Reservation> {
        self.check_parent(cp, &self.checked_latest(cp).await?)?;
        let previous = self.submissions()?.read()?.context("submission reservation disappeared")?;
        let obsolete = if previous.checkpoint == *cp {
            previous.reused_through.as_ref().context("consumed reservation has no reuse provenance")?
        } else {
            &previous.checkpoint
        };
        self.check_finalized_parent(cp, obsolete).await?;
        self.check_free_nonce(nonce).await?;
        self.submissions()?.reserve(cp.clone(), nonce, reused_through)
    }

    /// Moves a checkpoint to the next nonce once its own send consumed the reserved one without
    /// committing it, as a send in flight when the guardian retires the signer does: finalized
    /// history must show this exact call as the consumer, and the vault at that block must still
    /// lack the checkpoint.
    /// Error cases:
    /// - the consumption is not final, its consumer is another call, the checkpoint was accepted,
    ///   or the parent, authority or nonce checks fail
    async fn reserve_after_revert(
        &self,
        cp: &Checkpoint,
        previous: &Reservation,
        nonce: u64,
    ) -> anyhow::Result<Reservation> {
        let (anchor, consumed) = self.verify_consumer(previous, cp).await?;
        // A reservation without reuse provenance was alone at its nonce; its consumer is its call.
        ensure!(consumed == cp.seq, "the reserved nonce was consumed by another checkpoint");
        let vault = Address::from(self.domain.vault.0);
        let call = hasCheckpointAtCall { seq: cp.seq }.abi_encode();
        let accepted =
            hasCheckpointAtCall::abi_decode_returns(&self.call(vault, call, BlockId::hash_canonical(anchor)).await?)?;
        ensure!(!accepted, "the reserved checkpoint is accepted; await its canonical inclusion");
        self.check_parent(cp, &self.checked_latest(cp).await?)?;
        self.check_free_nonce(nonce).await?;
        self.submissions()?.reserve(cp.clone(), nonce, None)
    }
}

/// Encodes the exact enclave-authorized vault call.
/// Error cases:
/// - a parent, root, or signature is absent or malformed
pub(super) fn checkpoint_input(cp: &Checkpoint) -> anyhow::Result<Vec<u8>> {
    let parent = cp.parent.context("legacy checkpoint cannot be posted to a v2 vault")?;
    Ok(postCheckpointCall {
        seq: cp.seq,
        root: cp.root.parse()?,
        hasParent: parent.has_checkpoint,
        parentSeq: parent.seq,
        parentRoot: parent.root.0.into(),
        signature: cp.signature.parse()?,
    }
    .abi_encode())
}

/// Compares all transaction fields that identify one reserved checkpoint send.
/// Error cases:
/// - conflicting input/data aliases occur
fn same_transaction(left: &TransactionRequest, right: &TransactionRequest) -> anyhow::Result<bool> {
    Ok(left.from == right.from
        && left.to == right.to
        && left.nonce == right.nonce
        && left.chain_id == right.chain_id
        && left.value.unwrap_or_default() == right.value.unwrap_or_default()
        && left.input.unique_input()? == right.input.unique_input()?)
}
