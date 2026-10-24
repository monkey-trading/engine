//! Canonical block-pinned collateral, payout, and checkpoint-authority reads.

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, U64};
use alloy::rpc::types::Block;
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};

use crate::checkpoints::rpc::{
    CheckpointRpc, balanceOfCall, checkpointEpochAtCall, checkpointRootsCall, checkpointSignerAtCall,
    checkpointSignerCall, depositCountCall, hasCheckpointAtCall, hasCheckpointCall, latestRootCall, latestSeqCall,
    signerAtEpochCall, signerEpochCall, totalWithdrawnCall, usdcCall, usedNoncesCall,
};
use crate::checkpoints::{
    APPROVAL_PAGE_SIZE, AcceptedCheckpoint, ApprovalId, ApprovalStatus, LifecycleEvidence, Observation, VaultState,
    View,
};
use crate::custody::usdc_to_fixed;

impl CheckpointRpc {
    /// Samples one canonical block and pins every lifecycle and collateral read to its hash.
    /// Error cases:
    /// - ancestry, deployment, token, page bounds, RPC responses, or numeric conversions fail
    pub(super) async fn read_observation(&self, view: View, approvals: &[ApprovalId]) -> anyhow::Result<Observation> {
        ensure!(approvals.len() <= APPROVAL_PAGE_SIZE, "approval query exceeds page size");
        let chain_id: u64 = self.deadline(self.client.request::<_, U64>("eth_chainId", ())).await?.to();
        ensure!(chain_id == self.domain.chain_id, "checkpoint endpoint is on another chain");
        let tag = if view == View::Latest { BlockNumberOrTag::Latest } else { self.finality_tag };
        let tagged = self
            .deadline(self.client.request::<_, Option<Block>>("eth_getBlockByNumber", (tag, false)))
            .await?
            .context("checkpoint RPC has no finality block")?;
        // Follow immutable parent hashes from the exact sampled tag. A by-number read could
        // switch forks between requests and give an unconfirmed replacement the old tag's depth.
        let mut block = tagged;
        if view == View::Finalized {
            ensure!(block.header.number >= self.confirmations, "checkpoint chain has insufficient confirmation depth");
            for _ in 0..self.confirmations {
                let parent = self
                    .deadline(
                        self.client
                            .request::<_, Option<Block>>("eth_getBlockByHash", (block.header.parent_hash, false)),
                    )
                    .await?
                    .context("checkpoint confirmation ancestor is missing")?;
                ensure!(
                    parent.header.hash == block.header.parent_hash
                        && parent.header.number.checked_add(1) == Some(block.header.number),
                    "checkpoint confirmation ancestry is inconsistent"
                );
                block = parent;
            }
        }
        let at = BlockId::hash_canonical(block.header.hash);
        let vault = Address::from(self.domain.vault.0);
        let token = usdcCall::abi_decode_returns(&self.call(vault, usdcCall {}.abi_encode(), at).await?)?;
        ensure!(token == self.token, "checkpoint vault holds another token");
        let signer = checkpointSignerCall::abi_decode_returns(
            &self.call(vault, checkpointSignerCall {}.abi_encode(), at).await?,
        )?;
        let has =
            hasCheckpointCall::abi_decode_returns(&self.call(vault, hasCheckpointCall {}.abi_encode(), at).await?)?;
        let checkpoint = if has {
            let seq = latestSeqCall::abi_decode_returns(&self.call(vault, latestSeqCall {}.abi_encode(), at).await?)?;
            let root =
                latestRootCall::abi_decode_returns(&self.call(vault, latestRootCall {}.abi_encode(), at).await?)?;
            Some((seq, format!("{root:#x}")))
        } else {
            None
        };
        let balance = balanceOfCall::abi_decode_returns(
            &self.call(token, balanceOfCall { account: vault }.abi_encode(), at).await?,
        )?;
        let deposits =
            depositCountCall::abi_decode_returns(&self.call(vault, depositCountCall {}.abi_encode(), at).await?)?;
        let signer = engine_core::types::AccountId(signer.into());
        let signer_epoch =
            signerEpochCall::abi_decode_returns(&self.call(vault, signerEpochCall {}.abi_encode(), at).await?)?;
        ensure!(signer_epoch > 0, "vault signer epoch is zero");
        let paid =
            totalWithdrawnCall::abi_decode_returns(&self.call(vault, totalWithdrawnCall {}.abi_encode(), at).await?)?;
        let mut statuses = Vec::with_capacity(approvals.len());
        for approval in approvals {
            let claimed = usedNoncesCall::abi_decode_returns(
                &self
                    .call(
                        vault,
                        usedNoncesCall { account: Address::from(approval.account.0), nonce: approval.nonce }
                            .abi_encode(),
                        at,
                    )
                    .await?,
            )?;
            statuses.push(ApprovalStatus { approval: *approval, claimed });
        }
        let lifecycle = LifecycleEvidence {
            height: block.header.number,
            block_hash: block.header.hash,
            signer,
            signer_epoch,
            total_withdrawn: usdc_to_fixed(paid).context("vault paid total exceeds Fixed")?,
            checkpoint_seq: checkpoint.as_ref().map(|value| value.0),
            checkpoint_root: checkpoint.as_ref().map(|value| value.1.clone()),
            approvals: statuses,
        };
        Ok(Observation {
            view,
            state: VaultState {
                height: block.header.number,
                signer,
                checkpoint,
                balance: usdc_to_fixed(balance).context("vault balance exceeds Fixed")?,
                deposits: u64::try_from(deposits).context("vault deposit count exceeds u64")?,
            },
            lifecycle: Some(lifecycle),
        })
    }

    /// Reads immutable accepted metadata at the exact canonical block of a prior observation.
    /// Error cases:
    /// - no pinned lifecycle exists, metadata is absent, the block was orphaned, or RPC fails
    pub(super) async fn read_accepted_checkpoints(
        &self,
        at: &Observation,
        sequences: &[u64],
    ) -> anyhow::Result<Vec<AcceptedCheckpoint>> {
        ensure!(sequences.len() <= APPROVAL_PAGE_SIZE, "checkpoint authority query exceeds page size");
        ensure!(at.view == View::Finalized, "historical authority requires a finalized observation");
        let evidence = at.lifecycle.as_ref().context("checkpoint history requires a pinned canonical observation")?;
        let block = BlockId::hash_canonical(evidence.block_hash);
        let vault = Address::from(self.domain.vault.0);
        let mut accepted = Vec::with_capacity(sequences.len());
        for seq in sequences {
            ensure!(
                at.state.checkpoint.as_ref().is_some_and(|(latest, _)| seq <= latest),
                "checkpoint is beyond the finalized anchor"
            );
            ensure!(
                hasCheckpointAtCall::abi_decode_returns(
                    &self.call(vault, hasCheckpointAtCall { seq: *seq }.abi_encode(), block).await?
                )?,
                "checkpoint was not accepted by the vault"
            );
            let root = checkpointRootsCall::abi_decode_returns(
                &self.call(vault, checkpointRootsCall { seq: *seq }.abi_encode(), block).await?,
            )?;
            let signer = checkpointSignerAtCall::abi_decode_returns(
                &self.call(vault, checkpointSignerAtCall { seq: *seq }.abi_encode(), block).await?,
            )?;
            let signer_epoch = checkpointEpochAtCall::abi_decode_returns(
                &self.call(vault, checkpointEpochAtCall { seq: *seq }.abi_encode(), block).await?,
            )?;
            ensure!(
                signer != Address::ZERO && signer_epoch > 0 && signer_epoch <= evidence.signer_epoch,
                "accepted checkpoint authority is invalid"
            );
            accepted.push(AcceptedCheckpoint {
                seq: *seq,
                root: format!("{root:#x}"),
                signer: engine_core::types::AccountId(signer.into()),
                signer_epoch,
            });
        }
        Ok(accepted)
    }

    /// Authenticates payout signers independently of the epoch that accepted their proof root.
    /// Error cases:
    /// - an epoch is absent/future, the canonical block was orphaned, or RPC fails
    pub(super) async fn read_epoch_signers(
        &self,
        at: &Observation,
        epochs: &[u64],
    ) -> anyhow::Result<Vec<engine_core::types::AccountId>> {
        ensure!(epochs.len() <= APPROVAL_PAGE_SIZE, "signer epoch query exceeds page size");
        ensure!(at.view == View::Finalized, "signer history requires a finalized observation");
        let evidence = at.lifecycle.as_ref().context("signer history requires a pinned canonical observation")?;
        let block = BlockId::hash_canonical(evidence.block_hash);
        let vault = Address::from(self.domain.vault.0);
        let mut signers = Vec::with_capacity(epochs.len());
        for epoch in epochs {
            ensure!(*epoch > 0 && *epoch <= evidence.signer_epoch, "signer epoch is outside finalized history");
            let signer = signerAtEpochCall::abi_decode_returns(
                &self.call(vault, signerAtEpochCall { epoch: *epoch }.abi_encode(), block).await?,
            )?;
            ensure!(signer != Address::ZERO, "vault has no signer for the requested epoch");
            signers.push(engine_core::types::AccountId(signer.into()));
        }
        Ok(signers)
    }
}
