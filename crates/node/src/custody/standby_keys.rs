//! The strict-majority reads a member's standby key makes (`standby_key::chain`): every call at the
//! latest block a strict majority of the custody providers agrees on (`Quorum::posted_head`), with
//! that block's time from its agreed header. It has its own identity admission and quorum state, and
//! its alerts are Warnings under `quorum=standby_keys`: a lost read refuses a vouch or a standby
//! seal's opening, whose callers carry the impact.

use alloy::primitives::{Address, Bytes};
use anyhow::{Context, ensure};
use tokio::sync::{Mutex, MutexGuard};

use crate::config::CustodySection;
use crate::custody::evm::{BlockRef, VaultReader};
use crate::custody::quorum::Quorum;
use crate::custody::rpc::VaultRpc;
use crate::standby_key::chain::{AgreedBlock, StandbyChain};

/// The custody providers, read one session at a time.
pub(crate) struct StandbyReader<R = VaultRpc> {
    quorum: Quorum<R>,
    /// The LogHeads contract, whose read agrees the block.
    log_heads: Address,
    chain_id: u64,
    /// Held by each pinned block: another session's identity admission would clear this one's.
    session: Mutex<()>,
}

/// A block the providers agree on, and the session that pinned it.
pub(crate) struct ReaderBlock<'a, R> {
    reader: &'a StandbyReader<R>,
    _session: MutexGuard<'a, ()>,
    at: BlockRef,
    time: u64,
}

impl<R: VaultReader + Send + Sync> StandbyChain for StandbyReader<R> {
    type Block<'a>
        = ReaderBlock<'a, R>
    where
        Self: 'a;

    /// The highest block a strict majority has reached and agrees on, with its agreed header's time.
    /// Error cases:
    /// - the providers have no identity, block or header quorum, serve another chain, or hold no
    ///   vault
    async fn latest(&self) -> anyhow::Result<ReaderBlock<'_, R>> {
        let session = self.session.lock().await;
        let identity = self.quorum.identity().await?;
        ensure!(
            identity.chain_id == self.chain_id,
            "the custody quorum serves chain {}, not {}",
            identity.chain_id,
            self.chain_id
        );
        ensure!(identity.token.is_some(), "no vault answers on the custody quorum's chain");
        let (_, at) = self.quorum.posted_head(self.log_heads).await?;
        let headers = self.quorum.headers(at.number, at.number).await?;
        let header = headers.first().filter(|header| header.hash == at.hash).context("the agreed block's header")?;
        Ok(ReaderBlock { reader: self, _session: session, at, time: header.timestamp })
    }
}

impl<R: VaultReader + Send + Sync> AgreedBlock for ReaderBlock<'_, R> {
    /// The agreed header's time.
    fn time(&self) -> u64 {
        self.time
    }

    /// The call's return data, voted at the block under `operation`.
    /// Error cases:
    /// - no quorum
    async fn call(&self, operation: &'static str, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        self.reader.quorum.call_as(operation, self.at, to, data).await
    }
}

impl StandbyReader<VaultRpc> {
    /// Builds clients without accessing the network; `log_heads` is the LogHeads contract.
    /// Error cases:
    /// - EVM custody without its `[custody.evm]` section, fewer than three providers, or
    ///   `custody.vault` or a client does not parse
    pub(crate) fn connect(cfg: &CustodySection, log_heads: Address) -> anyhow::Result<Self> {
        let evm = cfg.evm.as_ref().context("standby keys need [custody.evm]")?;
        let quorum = Quorum::connect(evm, cfg.vault.parse().context("custody.vault")?)?.named("standby_keys", false);
        Ok(Self { quorum, log_heads, chain_id: cfg.chain_id, session: Mutex::new(()) })
    }
}

#[cfg(test)]
mod tests;
