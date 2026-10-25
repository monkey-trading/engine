//! The vault deposit watcher. Before V19 it credits the configured tag under a resumable host cursor.
//! V19 reconciles exact sequencer-confirmed receipts, locks their principal until Ethereum
//! finality, and recovers its finalised frontier from sequenced state (`evm/lifecycle.rs`).
//! Every chain, event-set and count read passes through the fixed provider quorum.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use anyhow::{anyhow, bail};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use tokio::sync::mpsc;
use tokio::task::spawn_blocking;
use tracing::{debug, info};

use crate::config::EvmCustodySection;
use crate::custody::chain::{BlockLink, ProvenChain};
use crate::custody::credit::{credit, moved_credits, sync};
use crate::custody::cursor::Cursor;
use crate::custody::sizing::AdaptiveSize;
use crate::custody::stall::{PollError, Polled, Stall};
use crate::custody::watchdog::Watchdog;
use crate::sequencer::Request;
mod legacy;
mod lifecycle;
mod progress;

/// Who a vault deposit credits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Beneficiary {
    Trader(AccountId),
    House,
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
    /// USDC base units the vault received.
    pub units: U256,
}

/// One block range's reads, checked against each other and the proven chain.
struct RangeRead {
    deposits: Vec<VaultDeposit>,
    count: u64,
    /// The proven hash of the range's last block, kept as the link for the next range.
    hash_to: B256,
}

pub struct DepositWatcher<V> {
    vault: V,
    start_block: u64,
    /// Fixed before polling, so queued activation cannot change a poll's command format.
    confirmed_deposits: bool,
    /// Volatile, canonically anchored evidence; disposition remains in the replayed core.
    confirmed: progress::ConfirmedProgress,
    /// The legacy credit `legacy_deposit_unproven` last named, so its episode is resolved once.
    legacy_unproven: Option<String>,
    /// The token the vault must hold.
    usdc: Address,
    pub(super) cursor: Cursor,
    cursor_path: PathBuf,
    /// Blocks the final block sits below the tagged block.
    confirmations: u64,
    pub(super) chain: ProvenChain,
    max_block_range: u64,
    /// Headers per batch.
    header_batch: AdaptiveSize,
    /// Blocks per log read.
    log_range: AdaptiveSize,
    pub(super) poll_interval: Duration,
    pub(super) stall_critical_after: Duration,
    watchdog: Watchdog,
}

impl VaultDeposit {
    /// The engine's reference for this deposit: `<tx hash>:<log index>`.
    pub fn chain_ref(&self) -> String {
        format!("{}:{}", self.tx_hash, self.log_index)
    }
}

impl<V: VaultReader> DepositWatcher<V> {
    /// A watcher resuming from the stored cursor, or from `cfg.start_block` without one.
    /// Error cases:
    /// - the cursor cannot be loaded (see `Cursor::load`)
    pub fn start(
        vault: V,
        cfg: &EvmCustodySection,
        chain_id: u64,
        vault_address: Address,
        log_seq: Option<u64>,
    ) -> anyhow::Result<Self> {
        let cursor = Cursor::load(&cfg.cursor_path, chain_id, vault_address, cfg.start_block, log_seq)?;
        Ok(Self::from_cursor(vault, cfg, cursor))
    }

    /// Builds the watcher; receipt mode takes no chain evidence from the host cursor.
    fn from_cursor(vault: V, cfg: &EvmCustodySection, cursor: Cursor) -> Self {
        info!(next_block = cursor.next_block, next_deposit_id = cursor.next_deposit_id, "deposit watcher ready");
        // The proof resumes from the last block read, so a change to it stalls rather than passes.
        let mut chain = ProvenChain::default();
        if !ProtocolVersion::LATEST.supports_deposit_finality() {
            if let (Some(hash), Some(last_read)) = (cursor.last_block_hash, cursor.next_block.checked_sub(1)) {
                chain.insert(last_read, hash);
            }
            if let Some((number, hash)) = cursor.crediting {
                chain.insert(number, hash);
            }
        }
        DepositWatcher {
            vault,
            start_block: cfg.start_block,
            confirmed_deposits: ProtocolVersion::LATEST.supports_deposit_finality(),
            confirmed: progress::ConfirmedProgress::default(),
            legacy_unproven: None,
            usdc: cfg.usdc,
            cursor,
            cursor_path: cfg.cursor_path.clone(),
            confirmations: cfg.confirmations,
            chain,
            max_block_range: cfg.max_block_range,
            header_batch: AdaptiveSize::new(cfg.max_block_range),
            log_range: AdaptiveSize::new(cfg.max_block_range),
            poll_interval: cfg.poll_interval,
            stall_critical_after: cfg.stall_critical_after,
            watchdog: Watchdog::new(cfg.stall_critical_after),
        }
    }

    /// Exercises historical watcher rules without exposing a production mode override.
    /// Error cases:
    /// - the legacy cursor cannot be loaded
    #[cfg(test)]
    pub(super) fn start_legacy(
        vault: V,
        cfg: &EvmCustodySection,
        chain_id: u64,
        address: Address,
        seq: Option<u64>,
    ) -> anyhow::Result<Self> {
        let mut watcher = Self::start(vault, cfg, chain_id, address, seq)?;
        watcher.confirmed_deposits = false;
        if let (Some(hash), Some(number)) = (watcher.cursor.last_block_hash, watcher.cursor.next_block.checked_sub(1)) {
            watcher.chain.insert(number, hash);
        }
        if let Some((number, hash)) = watcher.cursor.crediting {
            watcher.chain.insert(number, hash);
        }
        Ok(watcher)
    }

    /// Polls the admitted chain under the build's fixed command format, returning whether it has
    /// reached the eligible block; historical rules store a cursor and confirmed rules log progress.
    /// Error cases:
    /// - stalled: a chain read fails; the answers disagree in a way a retry may settle; or the
    ///   endpoint's chain or token, or the vault's count, contradicts the config or the cursor
    /// - fatal: the sequencer has stopped, or the cursor cannot be stored
    pub(super) async fn poll(&mut self, tx: &mpsc::Sender<Request>) -> Result<Polled, PollError> {
        if tx.is_closed() {
            return Err(PollError::Fatal(anyhow!("sequencer stopped")));
        }
        if self.confirmed_deposits {
            return self.poll_confirmed(tx).await;
        }
        // The vault, its token, the finality tag, the confirmations, and the start block all come
        // from the host-supplied config, whose SHA-256 the attestation binds, so a verifier checks
        // them against a published copy.
        let identity = self.vault.identity().await.map_err(PollError::rpc)?;
        let expected = VaultIdentity { chain_id: self.cursor.chain_id, token: Some(self.usdc) };
        if identity != expected {
            return Err(PollError::inconsistent(anyhow!(
                "the endpoint serves chain {} with vault token {:?}, not chain {} and token {}",
                identity.chain_id,
                identity.token,
                expected.chain_id,
                self.usdc
            )));
        }
        let finality = self.vault.finality().await.map_err(PollError::rpc)?;
        let final_block = finality.tagged.and_then(|tagged| tagged.number.checked_sub(self.confirmations));
        self.watchdog.observe(finality.head, finality.head_time, final_block, finality.head_support.as_ref());
        let (Some(tagged), Some(final_block)) = (finality.tagged, final_block) else {
            debug!(head = finality.head, "no block is final yet");
            return Ok(Polled::Waiting);
        };
        let from = self.cursor.next_block;
        if final_block < from {
            debug!(final_block, next_block = from, "waiting for the final block to reach the cursor");
            return Ok(Polled::Waiting);
        }
        let to = final_block.min(from.saturating_add(self.log_range.get() - 1));
        self.prove_chain(from, tagged, final_block).await?;
        let RangeRead { deposits, count, hash_to } = self.read_range(from, to).await?;
        // A transaction credited at a log index it no longer has was moved by a reorg below
        // finality; crediting its new reference would pay it twice.
        let moved = moved_credits(tx, &deposits).await.map_err(PollError::Fatal)?;
        if !moved.is_empty() {
            return Err(PollError::inconsistent(anyhow!("credited deposits {moved:?} have moved")));
        }
        if !deposits.is_empty() {
            // A range still being credited from before a restart keeps its higher anchor.
            self.cursor.crediting = match self.cursor.crediting {
                Some(crediting) if crediting.0 > to => Some(crediting),
                _ => Some((to, hash_to)),
            };
            self.store_cursor().await?;
        }
        for deposit in &deposits {
            credit(tx, deposit).await?;
        }
        // The cursor file is synced, so the credits it moves past must be on storage first.
        if !deposits.is_empty() {
            self.cursor.log_seq = sync(tx).await.map_err(PollError::Fatal)?;
        }
        self.cursor.next_block = to + 1;
        self.cursor.next_deposit_id = count;
        self.cursor.last_block_hash = Some(hash_to);
        self.cursor.crediting = self.cursor.crediting.filter(|(number, _)| *number > to);
        self.store_cursor().await?;
        self.chain.insert(to, hash_to);
        self.chain.prune_below(to);
        if to == final_block {
            self.log_range.reset_ceiling();
            self.header_batch.reset_ceiling();
        }
        self.log_range.succeeded();
        self.watchdog.observe_read(to == final_block, to + 1, final_block);
        debug!(from, to, deposits = deposits.len(), "vault blocks read");
        Ok(Polled::Read { caught_up: to == final_block })
    }

    /// One batch of headers, sized adaptively.
    /// Error cases:
    /// - stalled: the read fails
    async fn headers(&mut self, from: u64, to: u64) -> Result<Vec<BlockLink>, PollError> {
        match self.vault.headers(from, to).await {
            Ok(links) => {
                self.header_batch.succeeded();
                Ok(links)
            }
            Err(e) => {
                self.header_batch.failed();
                Err(PollError::rpc(e))
            }
        }
    }

    /// Stores the cursor off the async workers.
    /// Error cases:
    /// - fatal: the cursor cannot be stored
    async fn store_cursor(&self) -> Result<(), PollError> {
        let (cursor, path) = (self.cursor.clone(), self.cursor_path.clone());
        spawn_blocking(move || cursor.store(&path))
            .await
            .map_err(|e| PollError::Fatal(anyhow!("storing the deposit cursor: {e}")))?
            .map_err(PollError::Fatal)
    }

    /// Proves the chain up to `tagged`. A tag that names another block at a proven height, or a
    /// header that no longer links, is a reorg above the blocks already read, which the finality
    /// policy allows: the proof restarts once from the cursor, without an alert. Only a chain that
    /// still does not lead to the tag, such as one that rewrites a block already read, stalls.
    /// Error cases:
    /// - stalled: a header read fails, or the headers do not lead to `tagged` from the cursor
    async fn prove_chain(&mut self, from: u64, tagged: BlockRef, final_block: u64) -> Result<(), PollError> {
        if self.chain.get(tagged.number) == Some(tagged.hash) {
            return Ok(());
        }
        // A lagging backend reports an older tag: it is no reorg if it leads up to a proven block.
        if self.chain.top().is_some_and(|(top, _)| top > tagged.number) {
            match self.window_hashes(tagged.number, tagged.number).await {
                Ok(hashes) if hashes.get(&tagged.number) == Some(&tagged.hash) => return Ok(()),
                Err(e @ PollError::Stalled(Stall::Rpc(_))) => return Err(e),
                _ => {}
            }
        }
        // Blocks already read, or being credited, must not change; anything above may.
        let last_read = from.checked_sub(1).max(self.cursor.crediting.map(|(number, _)| number));
        match self.extend_chain(from, tagged, final_block).await {
            Err(PollError::Stalled(Stall::Unsettled(e))) => {
                debug!(tagged = tagged.number, "chain reorganised above the blocks read; proving again: {e:#}");
                self.chain.prune_above(last_read);
                self.extend_chain(from, tagged, final_block).await
            }
            result => result,
        }
    }

    /// Reads headers from just above the proven top (or from `from`) up to `tagged` in batches of
    /// the block range, links each to the one below, and once the last header is `tagged` keeps
    /// it, an anchor every `max_block_range` blocks, and every hash in the next read window. A
    /// proof that leaves this poll's read short of `final_block` counts toward the lag alert as it
    /// goes, since no deposit is read until it ends.
    /// Error cases:
    /// - stalled: a header read fails, a header does not link, or the last one is not `tagged`
    async fn extend_chain(&mut self, from: u64, tagged: BlockRef, final_block: u64) -> Result<(), PollError> {
        let top = self.chain.top();
        let mut run = self.chain.resume(top, top.map_or(from, |(number, _)| number + 1));
        if tagged.number.saturating_sub(run.next()) >= self.max_block_range {
            info!(from = run.next(), to = tagged.number, "proving a long stretch of chain before reading it");
        }
        let (every, window_end) = (self.max_block_range, from.saturating_add(self.max_block_range));
        let behind = final_block.saturating_sub(from) >= self.max_block_range;
        while run.next() <= tagged.number {
            let next = run.next();
            let end = tagged.number.min(next.saturating_add(self.header_batch.get() - 1));
            let links = match self.headers(next, end).await {
                Ok(links) => links,
                Err(e) => {
                    self.chain.interrupt(run);
                    return Err(e);
                }
            };
            run.extend(end, &links, |n| (n + 1).is_multiple_of(every), |n| n < window_end)
                .map_err(PollError::unsettled)?;
            if behind {
                self.watchdog.observe_read(false, from, final_block);
            }
        }
        if run.last() != Some((tagged.number, tagged.hash)) {
            return Err(PollError::unsettled(anyhow!(
                "the finality tag names block {} {}, but the chain read there ends at {:?}",
                tagged.number,
                tagged.hash,
                run.last()
            )));
        }
        self.chain.keep(run);
        self.chain.insert(tagged.number, tagged.hash);
        Ok(())
    }

    /// The hashes of blocks `from..=to`, proven: kept from the proof when it read them, or read
    /// up to the next anchor at or above `to`, each header linked to the one below, ending on that
    /// anchor's hash. Hashes commit to every ancestor, so that end fixes all of them. A read a
    /// failure interrupts resumes after the headers it linked; none of them is used until then.
    /// Error cases:
    /// - stalled: a header read fails, or the headers do not link up to the anchor
    async fn window_hashes(&mut self, from: u64, to: u64) -> Result<BTreeMap<u64, B256>, PollError> {
        if let Some(hashes) = self.chain.window(from, to) {
            return Ok(hashes);
        }
        let upper = self
            .chain
            .at_or_above(to)
            .ok_or_else(|| PollError::unsettled(anyhow!("block {to} is above the proven chain")))?;
        let mut run = self.chain.resume(None, from);
        while run.next() <= upper.0 {
            let next = run.next();
            let end = upper.0.min(next.saturating_add(self.header_batch.get() - 1));
            let links = match self.headers(next, end).await {
                Ok(links) => links,
                Err(e) => {
                    self.chain.interrupt(run);
                    return Err(e);
                }
            };
            run.extend(end, &links, |_| false, |_| true).map_err(PollError::unsettled)?;
        }
        if run.last() != Some(upper) {
            return Err(PollError::unsettled(anyhow!(
                "blocks {from}..={} end at {:?}, not the proven block {upper:?}",
                upper.0,
                run.last()
            )));
        }
        let hashes = run.window(from, to);
        self.chain.keep(run);
        Ok(hashes)
    }

    /// Reads and cross-checks blocks `from..=to` on the proven chain: their deposits, each from
    /// a proven block, and the vault's count in block `to`.
    /// Error cases:
    /// - stalled: a read fails, the answers disagree with each other or the proven chain, or the
    ///   vault counts fewer deposits than the cursor has read
    async fn read_range(&mut self, from: u64, to: u64) -> Result<RangeRead, PollError> {
        let hashes = self.window_hashes(from, to).await?;
        let hash_to = hashes[&to];
        let deposits = match self.vault.deposits(from, to).await {
            Ok(deposits) => deposits,
            Err(e) => {
                // Providers cap a log read's results; a smaller range gets under the cap.
                self.log_range.failed();
                return Err(PollError::rpc(e));
            }
        };
        let count = self.vault.deposit_count(BlockRef { number: to, hash: hash_to }).await.map_err(PollError::rpc)?;
        // Credits the chain no longer backs are an accounting mismatch: decision #15 alerts and
        // pauses withdrawals (refused under EVM custody anyway) while trading continues.
        if count < self.cursor.next_deposit_id {
            return Err(PollError::inconsistent(anyhow!(
                "the vault counts {count} deposits by block {to}, fewer than the {} already read",
                self.cursor.next_deposit_id
            )));
        }
        check_complete(&deposits, from, to, self.cursor.next_deposit_id, count).map_err(PollError::unsettled)?;
        if let Some(d) = deposits.iter().find(|d| hashes.get(&d.block) != Some(&d.block_hash)) {
            return Err(PollError::unsettled(anyhow!(
                "deposit {} is reported in block {} {}, which is not the proven block there",
                d.id,
                d.block,
                d.block_hash
            )));
        }
        Ok(RangeRead { deposits, count, hash_to })
    }
}

/// Checks `deposits` are every vault deposit in blocks `from..=to`: inside the range, with ids
/// running on from `next_id` with no gap or repeat, up to the vault's own `count` at `to`. A
/// lagging or partial RPC answer fails here instead of letting the cursor skip a deposit.
/// Error cases:
/// - a deposit outside the range, or ids that do not run from `next_id` to `count`
pub(super) fn check_complete(
    deposits: &[VaultDeposit],
    from: u64,
    to: u64,
    next_id: u64,
    count: u64,
) -> anyhow::Result<()> {
    if let Some(d) = deposits.iter().find(|d| d.block < from || d.block > to) {
        bail!("deposit {} reported in block {}, outside {from}..={to}", d.id, d.block);
    }
    let ids: Vec<u64> = deposits.iter().map(|d| d.id).collect();
    if !ids.iter().copied().eq(next_id..count) {
        bail!("the vault counts {count} deposits by block {to}, but its logs carry ids {ids:?} after {next_id}");
    }
    Ok(())
}
