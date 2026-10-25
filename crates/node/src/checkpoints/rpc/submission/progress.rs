//! Canonical transaction evidence for an obsolete checkpoint consuming a reused nonce.

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, B256, U64, U256};
use alloy::rpc::types::{Block, TransactionRequest};
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};
use serde::Deserialize;

use crate::checkpoints::View;
use crate::checkpoints::rpc::submission::Reservation;
use crate::checkpoints::rpc::submission::posting::checkpoint_input;
use crate::checkpoints::rpc::{CheckpointRpc, postCheckpointCall};
use crate::log::{Checkpoint, CheckpointParent};

#[derive(Debug, Deserialize)]
struct ConsumingBlock {
    hash: B256,
    number: U64,
    transactions: Vec<TransactionRequest>,
}

impl CheckpointRpc {
    /// Identifies a finalized nonce consumer without trusting a saved or returned transaction hash.
    /// Error cases:
    /// - canonical history is absent/inconsistent, finality lags, or the consumer is unrelated
    pub(super) async fn verify_consumer(&self, reservation: &Reservation, obsolete: &Checkpoint) -> anyhow::Result<()> {
        let mut anchor = self
            .deadline(self.client.request::<_, Option<Block>>("eth_getBlockByNumber", (self.finality_tag, false)))
            .await?
            .context("nonce reconciliation has no finality block")?;
        ensure!(anchor.header.number >= self.confirmations, "nonce reconciliation lacks confirmation depth");
        // Follow the sampled tag's immutable ancestry, exactly as the vault state reader does.
        for _ in 0..self.confirmations {
            let parent = self
                .deadline(
                    self.client.request::<_, Option<Block>>("eth_getBlockByHash", (anchor.header.parent_hash, false)),
                )
                .await?
                .context("nonce reconciliation confirmation ancestor is missing")?;
            ensure!(
                parent.header.hash == anchor.header.parent_hash
                    && parent.header.number.checked_add(1) == Some(anchor.header.number),
                "nonce reconciliation confirmation ancestry is inconsistent"
            );
            anchor = parent;
        }
        let final_nonce = self.nonce_at(anchor.header.hash).await?;
        ensure!(final_nonce > reservation.nonce, "reserved nonce consumption is not finalized");
        ensure!(final_nonce == reservation.nonce + 1, "unrelated finalized relayer nonce consumption");
        // Search nearby history first; a recent consumption does not need genesis state.
        let mut high = anchor.header.number;
        let mut distance = 1;
        let mut low = loop {
            let probe = anchor.header.number.saturating_sub(distance);
            if self.nonce_at_height(probe).await? <= reservation.nonce {
                break probe + 1;
            }
            ensure!(probe > 0, "reserved nonce predates canonical transaction history");
            high = probe;
            distance = distance.saturating_mul(2);
        };
        // At most 64 bisections for a u64 height. Number lookups discover a candidate only.
        while low < high {
            let mid = low + (high - low) / 2;
            if self.nonce_at_height(mid).await? > reservation.nonce {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        let block = self
            .deadline(
                self.client.request::<_, Option<Block>>("eth_getBlockByNumber", (BlockNumberOrTag::Number(low), false)),
            )
            .await?
            .context("nonce consuming block is missing")?;
        ensure!(
            block.header.number == low && low > 0 && low <= anchor.header.number,
            "nonce consuming block height mismatch"
        );
        ensure!(
            self.nonce_at(block.header.parent_hash).await? == reservation.nonce
                && self.nonce_at(block.header.hash).await? > reservation.nonce,
            "canonical block does not bracket the reserved nonce"
        );
        let full: ConsumingBlock = self
            .deadline(self.client.request::<_, Option<ConsumingBlock>>("eth_getBlockByHash", (block.header.hash, true)))
            .await?
            .context("nonce consuming transactions are missing")?;
        ensure!(
            full.hash == block.header.hash && full.number == U64::from(low),
            "nonce consuming block identity mismatch"
        );
        let mut matches =
            full.transactions.iter().filter(|tx| tx.from == Some(self.relayer) && tx.nonce == Some(reservation.nonce));
        let consumer = matches.next().context("canonical block has no reserved nonce consumer")?;
        ensure!(matches.next().is_none(), "canonical block has duplicate nonce consumers");
        self.authenticate_consumer(self.obsolete_call(consumer, obsolete)?, obsolete).await?;
        // An orphaned tag cannot lend its confirmation depth to a replacement by-number block.
        self.nonce_at(block.header.hash).await?;
        ensure!(
            self.nonce_at(anchor.header.hash).await? == final_nonce,
            "finalized nonce changed during reconciliation"
        );
        Ok(())
    }

    /// Reads an account nonce at an exact block that the endpoint must still consider canonical.
    /// Error cases:
    /// - historical state is unavailable, orphaned, or the RPC fails
    async fn nonce_at(&self, hash: B256) -> anyhow::Result<u64> {
        let value: U64 = self
            .deadline(self.client.request("eth_getTransactionCount", (self.relayer, BlockId::hash_canonical(hash))))
            .await?;
        Ok(value.to())
    }

    /// Uses a height only to discover an exact canonical nonce sample.
    /// Error cases:
    /// - historical blocks/state are missing, inconsistent, orphaned, or unavailable
    async fn nonce_at_height(&self, height: u64) -> anyhow::Result<u64> {
        let block = self
            .deadline(
                self.client
                    .request::<_, Option<Block>>("eth_getBlockByNumber", (BlockNumberOrTag::Number(height), false)),
            )
            .await?
            .context("nonce reconciliation history is missing")?;
        ensure!(block.header.number == height, "nonce reconciliation block height mismatch");
        self.nonce_at(block.header.hash).await
    }

    /// Authenticates the consuming checkpoint under its own signer. An earlier checkpoint in the
    /// obsolete prefix may predate a signer rotation, so its signer comes from the vault's accepted
    /// history for its seq, never from the obsolete checkpoint.
    /// Error cases:
    /// - the consumer is not the vault's accepted checkpoint at its seq, or its signature is invalid
    async fn authenticate_consumer(&self, mut consumed: Checkpoint, obsolete: &Checkpoint) -> anyhow::Result<()> {
        if consumed.seq != obsolete.seq {
            let at = self.read_observation(View::Finalized, &[]).await?;
            let accepted = self.read_accepted_checkpoints(&at, &[consumed.seq]).await?;
            let authority = accepted.first().context("nonce consumer has no accepted checkpoint authority")?;
            consumed.signer = authority.signer.to_string();
            authority.verify(&consumed)?;
            self.submissions()?.authorize(authority.clone())?;
        }
        self.submissions()?.validate_checkpoint(&consumed)
    }

    /// Decodes a vault call inside a prefix already made obsolete by actual finality.
    /// Error cases:
    /// - the nonce consumer is a foreign call or a checkpoint outside the obsolete prefix
    fn obsolete_call(&self, tx: &TransactionRequest, obsolete: &Checkpoint) -> anyhow::Result<Checkpoint> {
        ensure!(
            tx.to == Some(Address::from(self.domain.vault.0).into())
                && tx.chain_id == Some(self.domain.chain_id)
                && tx.value == Some(U256::ZERO),
            "reserved nonce was consumed by an unrelated transaction"
        );
        let input = tx.input.unique_input()?.context("nonce consumer has no checkpoint calldata")?;
        let call = postCheckpointCall::abi_decode_validate(input)?;
        ensure!(call.abi_encode().as_slice() == input.as_ref(), "nonce consumer has noncanonical checkpoint calldata");
        let cp = Checkpoint {
            seq: call.seq,
            root: format!("{:#x}", call.root),
            signature: format!("{:#x}", call.signature),
            signer: obsolete.signer.clone(),
            domain: Some(self.domain),
            parent: Some(CheckpointParent {
                has_checkpoint: call.hasParent,
                seq: call.parentSeq,
                root: engine_core::eip712::B256::from(call.parentRoot.0),
            }),
            deferred_withdrawals: obsolete.deferred_withdrawals,
        };
        ensure!(
            cp.seq <= obsolete.seq && (cp.seq != obsolete.seq || cp.root == obsolete.root),
            "reserved nonce consumer is outside the obsolete checkpoint prefix"
        );
        if cp.seq == obsolete.seq {
            ensure!(
                input.as_ref() == checkpoint_input(obsolete)?.as_slice(),
                "nonce consumer differs from the exact obsolete checkpoint"
            );
        }
        Ok(cp)
    }
}
