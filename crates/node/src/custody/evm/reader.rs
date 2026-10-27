//! What the deposit watcher reads from the chain: the deposit events and the reader trait.

use std::future::Future;

use alloy::primitives::{B256, U256};

use engine_core::types::AccountId;

use super::{BlockLink, BlockRef, Finality, VaultIdentity};

/// Who a vault deposit credits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Beneficiary {
    Trader(AccountId),
    House,
}

/// A vault deposit event, read at an exact log position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultDeposit {
    pub block: u64,
    pub block_hash: B256,
    pub tx_hash: B256,
    pub log_index: u64,
    /// The vault's id for it; trader and house deposits share one sequence.
    pub id: u64,
    pub beneficiary: Beneficiary,
    /// The token transfer's actual funding address; absent in legacy reads.
    pub payer: Option<AccountId>,
    /// USDC base units the vault received.
    pub units: U256,
}

/// What the watcher needs from the chain.
pub trait VaultReader {
    /// The chain the endpoint serves and the token the vault holds.
    fn identity(&self) -> impl Future<Output = anyhow::Result<VaultIdentity>> + Send;

    /// The head, and the block the finality tag names.
    fn finality(&self) -> impl Future<Output = anyhow::Result<Finality>> + Send;

    /// The head and the separate withdrawal finality tag.
    fn withdrawal_finality(&self) -> impl Future<Output = anyhow::Result<Finality>> + Send {
        self.finality()
    }

    /// The headers of blocks `from..=to`, in order.
    fn headers(&self, from: u64, to: u64) -> impl Future<Output = anyhow::Result<Vec<BlockLink>>> + Send;

    /// Every vault deposit event in blocks `from..=to`, in chain order.
    fn deposits(&self, from: u64, to: u64) -> impl Future<Output = anyhow::Result<Vec<VaultDeposit>>> + Send;

    /// The vault's deposit count in block `at`, which the endpoint must hold as canonical.
    fn deposit_count(&self, at: BlockRef) -> impl Future<Output = anyhow::Result<u64>> + Send;
}
