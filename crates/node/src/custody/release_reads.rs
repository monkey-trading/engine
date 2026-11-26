//! The strict-majority reads the holder of the enclave's keys confirms its checkpoint endpoint with
//! before it hands the keys, or its delivered secrets, on (`handover::quorum`): every call at a block
//! a strict majority of the custody providers agrees on, the endpoint's own finalized block once the
//! providers hold it final, or their agreed head. It has its own identity admission and quorum state,
//! one read at a time, and its alerts are Warnings under `quorum=handover_release`: a lost read refuses
//! the handover, which the successor retries.

use std::time::Duration;

use alloy::primitives::Address;
use anyhow::{Context, bail, ensure};
use engine_core::types::Millis;
use futures_util::future::BoxFuture;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::checkpoints::Observation;
use crate::config::CustodySection;
use crate::custody::evm::{BlockRef, VaultReader};
use crate::custody::quorum::Quorum;
use crate::custody::rpc::VaultRpc;
use crate::custody::standby_keys::QuorumBlock;
use crate::handover::quorum::{Held, ReleaseQuorum, held_at, page_disagreement, rotation_at};

/// How long a read waits for the providers' finality to reach a block the checkpoint endpoint already
/// holds final: each reads the same tag, so they differ by the moments their nodes take it in.
const FINAL_WAIT: Duration = Duration::from_secs(60);
/// How often it reads their finality meanwhile.
const FINAL_POLL: Duration = Duration::from_secs(2);

/// The custody providers, read one session at a time, for the vault `vault`.
pub(crate) struct ReleaseReader<R = VaultRpc> {
    quorum: Quorum<R>,
    chain_id: u64,
    vault: Address,
    /// Held by each read: another read's identity admission would clear this one's.
    session: Mutex<()>,
}

impl ReleaseReader<VaultRpc> {
    /// Builds clients without accessing the network.
    /// Error cases:
    /// - EVM custody without its `[custody.evm]` section, fewer than three providers, or
    ///   `custody.vault` or a client does not parse
    pub(crate) fn connect(cfg: &CustodySection) -> anyhow::Result<Self> {
        let evm = cfg.evm.as_ref().context("a holder's handover reads need [custody.evm]")?;
        let vault = cfg.vault.parse().context("custody.vault")?;
        Ok(Self::over(Quorum::connect(evm, vault)?, cfg.chain_id, vault))
    }
}

impl<R: VaultReader + Send + Sync> ReleaseReader<R> {
    /// A reader over `quorum`, for `vault` on `chain_id`.
    fn over(quorum: Quorum<R>, chain_id: u64, vault: Address) -> Self {
        Self { quorum: quorum.named("handover_release", false), chain_id, vault, session: Mutex::new(()) }
    }

    /// `at`, once a strict majority names its hash at its height and, within `FINAL_WAIT`, holds a block
    /// at or above that height final by the withdrawal finality tag, the tag the checkpoint endpoint's
    /// finalized reads take.
    /// Error cases:
    /// - the providers have no identity, finality or header quorum, or serve another chain
    /// - they hold no block at `at`'s height final in time, or name another hash there, which pages
    ///   `checkpoint_endpoint_disagrees`
    async fn final_block(&self, at: BlockRef) -> anyhow::Result<QuorumBlock<'_, R>> {
        self.ensure_chain().await?;
        let deadline = Instant::now() + FINAL_WAIT;
        loop {
            let held = self.quorum.withdrawal_finality().await?.tagged;
            if held.is_some_and(|held| held.number >= at.number) {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "the custody providers hold no block at height {} final, which the checkpoint endpoint holds final",
                at.number
            );
            tokio::time::sleep(FINAL_POLL).await;
        }
        let headers = self.quorum.headers(at.number, at.number).await?;
        let named = headers.first().filter(|header| header.number == at.number).context("the providers' header")?.hash;
        if named != at.hash {
            page_disagreement(format_args!(
                "the checkpoint endpoint holds block {} final at height {}, but the custody providers name block {named} \
                 there",
                at.hash, at.number
            ));
            bail!("the custody providers name another block at height {}", at.number);
        }
        QuorumBlock::at(&self.quorum, at).await
    }

    /// The highest block a strict majority has reached, by its agreed header.
    /// Error cases:
    /// - the providers have no identity, finality or header quorum, or serve another chain
    async fn head(&self) -> anyhow::Result<QuorumBlock<'_, R>> {
        self.ensure_chain().await?;
        let number = self.quorum.finality().await?.head;
        let headers = self.quorum.headers(number, number).await?;
        let header = headers.first().filter(|header| header.number == number).context("the agreed head's header")?;
        QuorumBlock::at(&self.quorum, BlockRef { number, hash: header.hash }).await
    }

    /// Admits the providers that agree on the chain, which must be the configured one.
    /// Error cases:
    /// - the providers have no identity quorum, or serve another chain
    async fn ensure_chain(&self) -> anyhow::Result<()> {
        let identity = self.quorum.identity().await?;
        ensure!(
            identity.chain_id == self.chain_id,
            "the custody quorum serves chain {}, not {}",
            identity.chain_id,
            self.chain_id
        );
        Ok(())
    }
}

impl<R: VaultReader + Send + Sync> ReleaseQuorum for ReleaseReader<R> {
    fn held<'a>(&'a self, at: &'a Observation, now_ms: Option<Millis>) -> BoxFuture<'a, anyhow::Result<Held>> {
        Box::pin(async move {
            let _session = self.session.lock().await;
            let block = self.final_block(pinned(at)?).await?;
            held_at(&block, self.vault, now_ms).await
        })
    }

    fn rotation<'a>(&'a self, at: &'a Observation) -> BoxFuture<'a, anyhow::Result<(bool, bool)>> {
        Box::pin(async move {
            let _session = self.session.lock().await;
            let at_finality = rotation_at(&self.final_block(pinned(at)?).await?, self.vault).await?;
            let at_head = rotation_at(&self.head().await?, self.vault).await?;
            Ok((at_finality, at_head))
        })
    }
}

/// The block `at`, a finalized observation, pinned.
/// Error cases:
/// - the observation pins no canonical block
fn pinned(at: &Observation) -> anyhow::Result<BlockRef> {
    let evidence = at.lifecycle.as_ref().context("the handover's reads require a pinned canonical observation")?;
    Ok(BlockRef { number: evidence.height, hash: evidence.block_hash })
}

#[cfg(test)]
mod tests;
