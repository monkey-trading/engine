//! What the deposit watcher reads from the chain: the reader trait and its value types.

use std::future::Future;

use alloy::primitives::{Address, B256, U256};
use engine_core::types::AccountId;

use crate::custody::chain::BlockLink;

/// Who a vault deposit credits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Beneficiary {
    Trader(AccountId),
    House,
}

/// What the deposit watcher needs from the chain to mirror staking events: the events of the
/// configured staking contract, decoded like the vault's.
pub use crate::custody::staking::{StakingEvent, StakingEventKind};

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

    /// Every staking event in the contiguous proven hash window, in chain order.
    /// Each log query must name its proven block hash, including empty responses.
    fn staking_events(&self, blocks: &[BlockRef]) -> impl Future<Output = anyhow::Result<Vec<StakingEvent>>> + Send;
}

/// A block by height and hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRef {
    pub number: u64,
    pub hash: B256,
}

/// Identity-qualified head observations for a fixed configured provider population.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadSupport {
    pub required: usize,
    pub heads: Vec<Option<u64>>,
}

/// The chain's head, and the block the finality tag names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finality {
    pub head: u64,
    /// The head block's timestamp, in seconds since the epoch.
    pub head_time: u64,
    /// `None` while the chain has no block at the tag yet.
    pub tagged: Option<BlockRef>,
    /// Per-provider diagnostics; raw single-provider readers leave this unset.
    pub head_support: Option<HeadSupport>,
}

/// Which chain an endpoint serves and which token its vault holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VaultIdentity {
    pub chain_id: u64,
    /// `None` when nothing answers at the vault address on that chain.
    pub token: Option<Address>,
}

/// One vault deposit event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultDeposit {
    pub block: u64,
    pub block_hash: B256,
    pub tx_hash: B256,
    pub log_index: u64,
    /// The vault's id for it; trader and house deposits share one sequence.
    pub id: u64,
    pub beneficiary: Beneficiary,
    /// The actual vault caller; absent for legacy events.
    pub payer: Option<AccountId>,
    /// USDC base units the vault received.
    pub units: U256,
}

impl VaultDeposit {
    /// The engine's reference for this deposit: `<tx hash>:<log index>`.
    pub fn chain_ref(&self) -> String {
        format!("{}:{}", self.tx_hash, self.log_index)
    }
}
