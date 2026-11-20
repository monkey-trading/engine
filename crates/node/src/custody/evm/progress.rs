//! The confirmed scan's progress and its reorganisation checks.
//!
//! Each window of blocks is read once, from the provider quorum: its deposits together with the hash of
//! its last block, the vault's count in that block, and the header of every block a deposit is in. The
//! window's last block becomes the boundary. Each poll reads the boundary it started from again and
//! compares its hash with the one stored: each provider reads it after its own log read, and the quorum
//! votes on the two as one answer, so no majority holds a window from one branch over a boundary from
//! another. A block's hash commits to its parent's, and so to every block below it, so a reorganisation at
//! or below the boundary changes it; one above changes nothing the scan had read. A window joins the scan
//! only once its boundary holds. On a mismatch the scan walks back through the stored window ends to the
//! highest that still matches, and reads again from there.
//!
//! Finalisation reads the frontier's blocks once they are final, which no reorganisation can change: the
//! header of every block a receipt it releases is in, and the vault's count at the frontier, so nothing a
//! reorganisation between a poll's reads let through is ever released.

use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::{Address, B256};
use anyhow::anyhow;
use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt};
use engine_core::types::AccountId;
use futures_util::future::try_join_all;
use tracing::info;

use crate::custody::evm::lifecycle::receipt;
use crate::custody::evm::{BlockRef, DepositWatcher, VaultReader, VaultRecord, check_complete};
use crate::custody::quorum::last_header;
use crate::custody::stall::PollError;
use crate::custody::usdc_to_fixed;

/// How many deposit blocks' headers, or deposits' records, are read at once.
const HEADER_READS: usize = 8;

#[derive(Default)]
pub(super) struct ConfirmedProgress {
    pub(super) scan: Option<ConfirmedScan>,
}

/// A window read and checked, waiting for its boundary to hold before it joins the scan.
pub(super) struct Window {
    end: BlockRef,
    count: u64,
    receipts: Vec<(DepositReceipt, Option<AccountId>)>,
    hashes: BTreeMap<u64, B256>,
    /// The boundary's hash, as the window's answer holds it.
    pub(super) boundary: Option<B256>,
}

pub(super) struct ConfirmedScan {
    anchor: Option<CustodyCheckpoint>,
    start: u64,
    start_count: u64,
    pub(super) end: Option<BlockRef>,
    count: u64,
    pub(super) canonical: Vec<DepositReceipt>,
    /// Each canonical receipt's vault caller, by exact reference, for access screening.
    pub(super) payers: BTreeMap<String, Option<AccountId>>,
    pub(super) hashes: BTreeMap<u64, B256>,
    pub(super) applied: usize,
    pub(super) finalised: usize,
}

impl ConfirmedScan {
    /// Starts at the sequenced frontier, without importing a host cursor.
    fn new(anchor: Option<CustodyCheckpoint>, start: u64, count: u64) -> Self {
        Self {
            anchor,
            start,
            start_count: count,
            end: None,
            count,
            canonical: Vec::new(),
            payers: BTreeMap::new(),
            hashes: BTreeMap::new(),
            applied: 0,
            finalised: 0,
        }
    }

    /// The first unread block; every preceding window was fully checked.
    pub(super) fn next(&self) -> u64 {
        self.end.map_or(self.start, |end| end.number + 1)
    }

    /// The verified count at a height covered by this complete scan.
    pub(super) fn count_at(&self, block: u64) -> u64 {
        self.start_count + self.canonical.partition_point(|receipt| receipt.block <= block) as u64
    }

    /// Drops everything above `kept`, a window end of this scan, which becomes its end.
    fn truncate(&mut self, kept: BlockRef) {
        let keep = self.canonical.partition_point(|receipt| receipt.block <= kept.number);
        self.count = self.count_at(kept.number);
        self.canonical.truncate(keep);
        self.applied = self.applied.min(keep);
        self.finalised = self.finalised.min(keep);
        self.hashes.retain(|block, _| *block <= kept.number);
        self.hashes.insert(kept.number, kept.hash);
        self.payers.split_off(&format!("{:020}:", kept.number + 1));
        self.end = Some(kept);
    }
}

impl<V: VaultReader> DepositWatcher<V> {
    /// Compares `boundary`, the highest stored block above the finalised frontier when the poll started,
    /// with its hash now: `observed`, from the poll's window answer, or else a read of its header. On a
    /// mismatch, walks back through the stored window ends below it, by halves, to the highest that still
    /// matches, drops everything above that, and leaves the scan to read again from there; a walk that a
    /// failed read stops drops everything above the finalised frontier `floor` instead, so a mismatch once
    /// seen is never forgotten. Returns whether the boundary held.
    /// Error cases:
    /// - stalled: a header read fails or lacks a quorum
    pub(super) async fn check_boundary(
        &mut self,
        floor: Option<u64>,
        boundary: Option<(u64, B256)>,
        observed: Option<B256>,
    ) -> Result<bool, PollError> {
        let Some((top, hash)) = boundary else { return Ok(true) };
        let current = match observed {
            Some(current) => current,
            None => self.header_at(top).await?.hash,
        };
        if current == hash {
            return Ok(true);
        }
        let kept = match self.walk_back(floor, top).await {
            Ok(kept) => kept,
            Err(cause) => {
                self.chain.prune_above(floor);
                self.confirmed.scan = None;
                return Err(cause);
            }
        };
        info!(
            boundary = top,
            kept = ?kept.map(|(number, _)| number).or(floor),
            "the chain reorganised at or below the scanned boundary; scanning again above the highest end that matches"
        );
        self.chain.prune_above(kept.map(|(number, _)| number).or(floor));
        match (kept, &mut self.confirmed.scan) {
            // A stored end above everything the scan holds leaves it as it is.
            (Some((number, _)), Some(scan)) if scan.end.is_none_or(|end| number > end.number) => {}
            (Some((number, hash)), Some(scan)) if number >= scan.start => scan.truncate(BlockRef { number, hash }),
            _ => self.confirmed.scan = None,
        }
        Ok(false)
    }

    /// The highest stored window end above `floor` and below `top` that still matches its block, found by
    /// halves: those below the fork match and those above it do not.
    /// Error cases:
    /// - stalled: a header read fails or lacks a quorum
    async fn walk_back(&self, floor: Option<u64>, top: u64) -> Result<Option<(u64, B256)>, PollError> {
        let ends = self.chain.between(floor, top);
        let (mut low, mut high) = (0, ends.len());
        while low < high {
            let middle = low + (high - low) / 2;
            let (number, hash) = ends[middle];
            if self.header_at(number).await?.hash == hash {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(low.checked_sub(1).map(|index| ends[index]))
    }

    /// Block `number`'s header, read through the quorum.
    /// Error cases:
    /// - stalled: the read fails, lacks a quorum, or answers for another block
    pub(super) async fn header_at(&self, number: u64) -> Result<crate::custody::chain::BlockLink, PollError> {
        let headers = self.vault.headers(number, number).await.map_err(PollError::rpc)?;
        last_header(headers, number).map_err(PollError::rpc)
    }

    /// The hashes of `blocks`, each read through the quorum, a few at a time.
    /// Error cases:
    /// - stalled: a read fails, lacks a quorum, or answers for another block
    pub(super) async fn block_hashes(&self, blocks: &BTreeSet<u64>) -> Result<BTreeMap<u64, B256>, PollError> {
        let blocks: Vec<u64> = blocks.iter().copied().collect();
        let mut hashes = BTreeMap::new();
        for chunk in blocks.chunks(HEADER_READS) {
            let headers = try_join_all(chunk.iter().map(|&number| self.header_at(number))).await?;
            hashes.extend(headers.into_iter().map(|header| (header.number, header.hash)));
        }
        Ok(hashes)
    }

    /// Discards all unfinalised evidence after a canonical conflict, retaining only core finality.
    fn reset_confirmed_chain(&mut self, from: u64) {
        self.chain.prune_above(from.checked_sub(1));
        self.confirmed = ConfirmedProgress::default();
    }

    /// Reuses complete windows only while their endpoint is still a stored boundary.
    /// Trims finalized receipts without discarding the independently verified suffix.
    /// Error cases:
    /// - the core frontier's count contradicts the complete canonical scan
    pub(super) fn resume_confirmed_scan(
        &mut self,
        checkpoint: Option<CustodyCheckpoint>,
        from: u64,
        count: u64,
    ) -> Result<(), PollError> {
        if self
            .confirmed
            .scan
            .as_ref()
            .is_some_and(|scan| scan.end.is_some_and(|end| self.chain.get(end.number) != Some(end.hash)))
        {
            self.confirmed.scan = None;
        }
        if let Some(scan) = &mut self.confirmed.scan
            && scan.anchor != checkpoint
        {
            if let Some(point) = checkpoint
                .filter(|point| scan.end.is_some_and(|end| point.block <= end.number) && point.block >= scan.start)
            {
                let removed = scan.canonical.partition_point(|receipt| receipt.block <= point.block);
                if scan.count_at(point.block) != point.deposit_count {
                    return Err(PollError::inconsistent(anyhow!(
                        "finalized frontier differs from the verified receipt count"
                    )));
                }
                scan.canonical.drain(..removed);
                scan.applied = scan.applied.saturating_sub(removed);
                scan.finalised = scan.finalised.saturating_sub(removed);
                scan.hashes.retain(|block, _| *block > point.block);
                scan.payers = scan.payers.split_off(&format!("{:020}:", point.block + 1));
                scan.anchor = checkpoint;
                scan.start = from;
                scan.start_count = count;
            } else {
                self.confirmed.scan = None;
            }
        }
        self.confirmed.scan.get_or_insert_with(|| ConfirmedScan::new(checkpoint, from, count));
        Ok(())
    }

    /// Reads and checks at most one window above the scan's end: its deposits, the hash of its last block
    /// and, with `boundary`, that block's hash after them, as one quorum answer; the vault's count in its
    /// last block; and the header of every block holding a deposit, each through the quorum. Nothing joins
    /// the scan until `commit_window`, once the boundary holds.
    /// Error cases:
    /// - reads fail or lack a quorum, or a deposit's block is not the canonical one there, or complete event
    ///   ids, counts or economics disagree
    pub(super) async fn read_window(
        &mut self,
        eligible: u64,
        boundary: Option<u64>,
    ) -> Result<Option<Window>, PollError> {
        let scan = self.confirmed.scan.as_ref().expect("scan initialized");
        let (next, scanned) = (scan.next(), scan.count);
        if next > eligible {
            return Ok(None);
        }
        let to = eligible.min(next.saturating_add(self.log_range.get() - 1));
        let ranged = match self.vault.ranged_deposits(next, to, boundary).await {
            Ok(ranged) => ranged,
            Err(cause) => {
                self.log_range.failed();
                return Err(PollError::rpc(cause));
            }
        };
        let end = BlockRef { number: to, hash: ranged.end };
        let count = self.vault.deposit_count(end).await.map_err(PollError::rpc)?;
        let blocks: BTreeSet<u64> = ranged.deposits.iter().map(|deposit| deposit.block).filter(|b| *b != to).collect();
        let mut hashes = self.block_hashes(&blocks).await?;
        hashes.insert(to, end.hash);
        check_complete(&ranged.deposits, next, to, scanned, count).map_err(PollError::unsettled)?;
        let mut ordinals = BTreeMap::<B256, u64>::new();
        let mut receipts = Vec::with_capacity(ranged.deposits.len());
        for deposit in &ranged.deposits {
            if hashes.get(&deposit.block) != Some(&deposit.block_hash) {
                return Err(PollError::unsettled(anyhow!(
                    "deposit {} is reported in block {} {}, which is not the canonical block there",
                    deposit.id,
                    deposit.block,
                    deposit.block_hash
                )));
            }
            let ordinal = ordinals.entry(deposit.tx_hash).or_default();
            receipts.push((receipt(deposit, *ordinal)?, deposit.payer));
            *ordinal += 1;
        }
        self.log_range.succeeded();
        Ok(Some(Window { end, count, receipts, hashes, boundary: ranged.boundary }))
    }

    /// Appends `window`, read by `read_window`, to the scan; its last block becomes the boundary.
    pub(super) fn commit_window(&mut self, window: Window) {
        let scan = self.confirmed.scan.as_mut().expect("scan retained");
        scan.hashes.extend(window.hashes);
        scan.payers.extend(window.receipts.iter().map(|(receipt, payer)| (receipt.reference(), *payer)));
        scan.canonical.extend(window.receipts.into_iter().map(|(receipt, _)| receipt));
        scan.end = Some(window.end);
        scan.count = window.count;
        self.chain.insert(window.end.number, window.end.hash);
    }

    /// Checks the scan against the block the release tag names, `released`, before receipts through it are
    /// released, with reads executed in that block by its hash, which the quorum voted with the tag: the
    /// vault's count there, and its record of every deposit a released receipt is, its payer, amount and
    /// beneficiary. The scan's windows were each read with the boundary below them as one answer, but
    /// nothing else binds them to the release tag's chain, so these reads do: a scan that a reorganisation
    /// between its reads, or providers answering from a branch the tag has left, made to differ from that
    /// chain is never released. The header of every block a released receipt is in must hold its hash, so
    /// a receipt that moved keeps no stale identity, and a window end stored at the release height must hold
    /// the tag's hash. On a mismatch the scan starts again from the finalised frontier, whose reconciliation
    /// then moves or reverses whatever the release tag's chain does not hold. Returns whether the scan held.
    /// Error cases:
    /// - stalled: a read fails or lacks a quorum
    pub(super) async fn verify_final(&mut self, from: u64, released: BlockRef) -> Result<bool, PollError> {
        let scan = self.confirmed.scan.as_ref().expect("scan retained");
        let releasing: Vec<(DepositReceipt, Option<AccountId>)> = scan.canonical[scan.finalised..]
            .iter()
            .take_while(|receipt| receipt.block <= released.number)
            .map(|receipt| (receipt.clone(), scan.payers.get(&receipt.reference()).copied().flatten()))
            .collect();
        let expected = scan.count_at(released.number);
        let counted = scan.end == Some(released);
        let mut records = Vec::with_capacity(releasing.len());
        for chunk in releasing.chunks(HEADER_READS) {
            let reads = chunk.iter().map(|(receipt, _)| self.vault.deposit_record(released, receipt.deposit_id));
            records.extend(try_join_all(reads).await.map_err(PollError::rpc)?);
        }
        let blocks: BTreeSet<u64> = releasing.iter().map(|(receipt, _)| receipt.block).collect();
        let hashes = self.block_hashes(&blocks).await?;
        let moved = releasing
            .iter()
            .zip(&records)
            .find(|((receipt, payer), record)| {
                !recorded(receipt, *payer, record) || hashes.get(&receipt.block) != Some(&receipt.block_hash)
            })
            .map(|((receipt, _), _)| receipt.reference());
        let count =
            if counted { expected } else { self.vault.deposit_count(released).await.map_err(PollError::rpc)? };
        let end_holds = self.chain.get(released.number).is_none_or(|hash| hash == released.hash);
        if moved.is_none() && count == expected && end_holds {
            return Ok(true);
        }
        info!(
            released = released.number,
            moved = ?moved,
            count,
            expected,
            end_holds,
            "the release tag's chain differs from the scan; reading again from the finalised frontier"
        );
        self.reset_confirmed_chain(from);
        Ok(false)
    }
}

/// Whether the vault's `record` is `receipt` as the scan read it, paid by `payer`, `None` for a booked
/// surplus: the same payer, amount and beneficiary.
fn recorded(receipt: &DepositReceipt, payer: Option<AccountId>, record: &VaultRecord) -> bool {
    let account = match receipt.beneficiary {
        DepositBeneficiary::Trader { account } => Address::from(account.0),
        DepositBeneficiary::House => Address::ZERO,
    };
    record.payer == payer.map_or(Address::ZERO, |payer| Address::from(payer.0))
        && record.account == account
        && usdc_to_fixed(record.units) == Some(receipt.amount)
}
