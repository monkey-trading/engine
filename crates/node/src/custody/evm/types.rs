//! What the deposit watcher reads from the chain: the reader trait and its value types.

use std::future::Future;

use alloy::primitives::{Address, B256, Bytes, U256};
use engine_core::types::AccountId;

use crate::custody::chain::BlockLink;

/// Who a vault deposit credits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Beneficiary {
    Trader(AccountId),
    House,
}

/// What the deposit watcher needs from the chain to mirror staking events: the events of the
/// configured staking contract and operator registry, decoded like the vault's.
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

    /// `deposits`, with the hash of block `to` as the same reader reports it, read first, so a reader
    /// that does not serve the range's last block refuses rather than answering it as empty; and, with
    /// `boundary`, that block's hash, read after the logs, so one answer holds both the window and the
    /// block it builds on.
    /// Error cases:
    /// - a read fails, or a header answers for another height
    fn ranged_deposits(
        &self,
        from: u64,
        to: u64,
        boundary: Option<u64>,
    ) -> impl Future<Output = anyhow::Result<RangedDeposits>> + Send {
        // Futures do nothing until awaited, so the reads run in this order.
        let (end, deposits) = (self.headers(to, to), self.deposits(from, to));
        let below = boundary.map(|number| (number, self.headers(number, number)));
        async move {
            let end = crate::custody::quorum::last_header(end.await?, to)?.hash;
            let deposits = deposits.await?;
            let boundary = match below {
                Some((number, read)) => Some(crate::custody::quorum::last_header(read.await?, number)?.hash),
                None => None,
            };
            Ok(RangedDeposits { end, deposits, boundary })
        }
    }

    /// The vault's deposit count in block `at`, which the endpoint must hold as canonical.
    fn deposit_count(&self, at: BlockRef) -> impl Future<Output = anyhow::Result<u64>> + Send;

    /// The vault's record of deposit `id` in block `at`, which the endpoint must hold as canonical. A
    /// reader that cannot read it refuses.
    fn deposit_record(&self, at: BlockRef, id: u64) -> impl Future<Output = anyhow::Result<VaultRecord>> + Send {
        let _ = (at, id);
        async { anyhow::bail!("this reader does not read deposit records") }
    }

    /// Every staking and operator registry event in blocks `from..=to.number`, in chain order, from one
    /// log read over the range. A quorum takes each provider's header at `to` with its log read, in one
    /// voted answer, and refuses one that reports another hash.
    fn staking_events(&self, from: u64, to: BlockRef)
    -> impl Future<Output = anyhow::Result<Vec<StakingEvent>>> + Send;

    /// `staking_events`, with the answer also bound to `release`, the block the release tag names, when it
    /// is above the range: a quorum takes each provider's header there after its log read, in the same
    /// voted answer, and refuses one that differs. A reader with one source answers as `staking_events`.
    fn released_staking_events(
        &self,
        from: u64,
        to: BlockRef,
        release: Option<BlockRef>,
    ) -> impl Future<Output = anyhow::Result<Vec<StakingEvent>>> + Send {
        let _ = release;
        self.staking_events(from, to)
    }

    /// The same events at each of `blocks`, in chain order, each read by its canonical block hash, so an
    /// empty answer is bound to that block. A reader that cannot read them refuses.
    fn pinned_staking_events(
        &self,
        blocks: &[BlockRef],
    ) -> impl Future<Output = anyhow::Result<Vec<StakingEvent>>> + Send {
        let _ = blocks;
        async { anyhow::bail!("this reader does not read staking events by block hash") }
    }

    /// The registry the configured staking contract records as its deployer, in the canonical
    /// block `at`, or `None` where the staking contract has no code yet, before its deployment.
    /// A reader that cannot read it refuses.
    fn staking_registry(&self, at: BlockRef) -> impl Future<Output = anyhow::Result<Option<Address>>> + Send {
        let _ = at;
        async { anyhow::bail!("this reader does not read the staking contract's registry") }
    }

    /// The schedule the configured registry names, in the canonical block `at`; the zero address
    /// before the timelock names one. A reader that cannot read it refuses.
    fn registry_schedule(&self, at: BlockRef) -> impl Future<Output = anyhow::Result<Address>> + Send {
        let _ = at;
        async { anyhow::bail!("this reader does not read the registry's schedule") }
    }

    /// The registry the configured schedule records, in the canonical block `at`, or `None` where
    /// the schedule has no code yet. A reader that cannot read it refuses.
    fn schedule_registry(&self, at: BlockRef) -> impl Future<Output = anyhow::Result<Option<Address>>> + Send {
        let _ = at;
        async { anyhow::bail!("this reader does not read the schedule's registry") }
    }

    /// The native balance of `account` in the canonical block `at`. A reader that cannot read it
    /// refuses.
    fn account_balance(&self, at: BlockRef, account: Address) -> impl Future<Output = anyhow::Result<U256>> + Send {
        let _ = (at, account);
        async { anyhow::bail!("this reader does not read account balances") }
    }

    /// The return data of a read-only call of `data` to the contract `to`, executed in the canonical
    /// block `at`. A reader that cannot make it refuses.
    fn contract_call(
        &self,
        at: BlockRef,
        to: Address,
        data: Bytes,
    ) -> impl Future<Output = anyhow::Result<Bytes>> + Send {
        let _ = (at, to, data);
        async { anyhow::bail!("this reader does not make contract calls") }
    }

    /// The newest `(seq, link)` the LogHeads contract at `contract` holds for the vault's current
    /// signer epoch, read in the canonical block `at`; `None` before the epoch's first post. A
    /// reader that cannot read it refuses.
    /// Error cases:
    /// - the calls fail or time out, or an answer does not decode
    fn log_head(&self, contract: Address, at: BlockRef) -> impl Future<Output = anyhow::Result<PostedHead>> + Send {
        let _ = (contract, at);
        async { anyhow::bail!("this reader does not read the log-head contract") }
    }

    /// The chain's latest block: its height and hash. A reader that cannot read it refuses.
    /// Error cases:
    /// - the request fails or times out, or the endpoint has no head
    fn latest_block(&self) -> impl Future<Output = anyhow::Result<(u64, B256)>> + Send {
        async { anyhow::bail!("this reader does not read the latest block") }
    }
}

/// The vault's current signer epoch and, for it, the newest head the LogHeads contract holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostedHead {
    pub epoch: u64,
    /// `(seq, link)`, or `None` before the epoch's first post.
    pub head: Option<(u64, B256)>,
}

/// The vault's record of one deposit: its funding caller, the zero address for a booked surplus; its USDC
/// base units; and its beneficiary, the zero address for the house's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VaultRecord {
    pub payer: Address,
    pub units: U256,
    pub account: Address,
}

/// One reader's answer for a window of blocks: its deposits, its last block's hash, and the hash of the
/// block it builds on, when asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct RangedDeposits {
    pub end: B256,
    pub deposits: Vec<VaultDeposit>,
    pub boundary: Option<B256>,
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
