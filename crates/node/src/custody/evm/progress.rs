//! The confirmed scan's progress and its reorganisation checks.
//!
//! Each window of blocks is read once, from the provider quorum: its deposits together with the hash of
//! its last block, the vault's count in that block, and the header of every block a deposit is in. The
//! window's last block becomes the boundary. Each poll, after its window read, reads the boundary the poll
//! started from again and compares its hash with the one stored. A block's hash commits to its parent's,
//! and so to every block below it, so a reorganisation at or below the boundary changes it; one above
//! changes nothing the scan had read. Read after the window, it also catches a reorganisation that lands
//! between the two reads. On a mismatch the scan drops the poll's window, walks back through the stored
//! window ends to the highest that still matches, and reads again from there.
//!
//! Finalisation reads the frontier's blocks once they are final, which no reorganisation can change: the
//! header of every block a receipt it releases is in, and the vault's count at the frontier, so nothing a
//! reorganisation between a poll's reads let through is ever released.

use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::B256;
use anyhow::anyhow;
use engine_core::deposits::{CustodyCheckpoint, DepositReceipt};
use engine_core::types::AccountId;
use tracing::info;

use crate::custody::evm::lifecycle::receipt;
use crate::custody::evm::{BlockRef, DepositWatcher, VaultDeposit, VaultReader, check_complete};
use crate::custody::quorum::last_header;
use crate::custody::stall::PollError;

#[derive(Default)]
pub(super) struct ConfirmedProgress {
    pub(super) scan: Option<ConfirmedScan>,
    raw: Option<RawWindow>,
}

/// A window's deposits and the hash of its last block, kept until its count reads, so a failed count read
/// or a cancelled poll does not read the logs again.
struct RawWindow {
    from: u64,
    to: u64,
    hash: B256,
    deposits: Vec<VaultDeposit>,
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
    /// Reads `boundary`, the highest stored block when the poll started, again and compares its hash with
    /// the stored one, unless it is the finalised frontier `floor`, which the poll has already read. On a
    /// mismatch, walks back through the stored window ends below it, by halves, to the highest that still
    /// matches, drops everything above that, the poll's own window included, and leaves the scan to read
    /// again from there. Returns whether the boundary held.
    /// Error cases:
    /// - stalled: a header read fails or lacks a quorum
    pub(super) async fn check_boundary(
        &mut self,
        floor: Option<u64>,
        boundary: Option<(u64, B256)>,
    ) -> Result<bool, PollError> {
        let Some((top, hash)) = boundary.filter(|(top, _)| floor.is_none_or(|floor| *top > floor)) else {
            return Ok(true);
        };
        if self.header_at(top).await?.hash == hash {
            return Ok(true);
        }
        // Every stored end above the frontier and below the boundary, lowest first. Those below the fork
        // match and those above it do not, so the highest match is found by halves.
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
        let kept = low.checked_sub(1).map(|index| ends[index]);
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
        self.confirmed.raw = None;
        Ok(false)
    }

    /// Block `number`'s header, read through the quorum.
    /// Error cases:
    /// - stalled: the read fails, lacks a quorum, or answers for another block
    pub(super) async fn header_at(&self, number: u64) -> Result<crate::custody::chain::BlockLink, PollError> {
        let headers = self.vault.headers(number, number).await.map_err(PollError::rpc)?;
        last_header(headers, number).map_err(PollError::rpc)
    }

    /// The hashes of `blocks`, each read through the quorum.
    /// Error cases:
    /// - stalled: a read fails, lacks a quorum, or answers for another block
    pub(super) async fn block_hashes(&self, blocks: &BTreeSet<u64>) -> Result<BTreeMap<u64, B256>, PollError> {
        let mut hashes = BTreeMap::new();
        for &number in blocks {
            hashes.insert(number, self.header_at(number).await?.hash);
        }
        Ok(hashes)
    }

    /// Discards all unfinalised evidence after a canonical conflict, retaining only core finality.
    pub(super) fn reset_confirmed_chain(&mut self, from: u64) {
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
            self.confirmed.raw = None;
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
                self.confirmed.raw = None;
            }
        }
        self.confirmed.scan.get_or_insert_with(|| ConfirmedScan::new(checkpoint, from, count));
        Ok(())
    }

    /// Appends at most one complete log window: its deposits and the hash of its last block, the vault's
    /// count in that block, and the header of every block holding a deposit, each through the quorum. The
    /// window's last block becomes the boundary. Logs awaiting their count remain private evidence, so
    /// quota errors and cancellation do not repeat completed reads.
    /// Error cases:
    /// - reads fail or lack a quorum, or a deposit's block is not the canonical one there, or complete event
    ///   ids, counts or economics disagree
    pub(super) async fn advance_confirmed_scan(&mut self, eligible: u64) -> Result<(), PollError> {
        let scan = self.confirmed.scan.as_ref().expect("scan initialized");
        let next = scan.next();
        if next > eligible {
            return Ok(());
        }
        let end = eligible.min(next.saturating_add(self.log_range.get() - 1));
        if self.confirmed.raw.as_ref().is_some_and(|raw| raw.from != next || raw.to > eligible) {
            self.confirmed.raw = None;
        }
        if self.confirmed.raw.is_none() {
            let (hash, deposits) = match self.vault.ranged_deposits(next, end).await {
                Ok(read) => read,
                Err(cause) => {
                    self.log_range.failed();
                    return Err(PollError::rpc(cause));
                }
            };
            self.confirmed.raw = Some(RawWindow { from: next, to: end, hash, deposits });
        }
        let raw = self.confirmed.raw.as_ref().expect("window retained");
        let window_end = BlockRef { number: raw.to, hash: raw.hash };
        let count = self.vault.deposit_count(window_end).await.map_err(PollError::rpc)?;
        let raw = self.confirmed.raw.as_ref().expect("window retained");
        let blocks: BTreeSet<u64> = raw.deposits.iter().map(|deposit| deposit.block).filter(|b| *b != raw.to).collect();
        let mut hashes = self.block_hashes(&blocks).await?;
        hashes.insert(window_end.number, window_end.hash);
        let raw = self.confirmed.raw.as_ref().expect("window retained");
        let mut ordinals = BTreeMap::<B256, u64>::new();
        let validated = check_complete(
            &raw.deposits,
            raw.from,
            raw.to,
            self.confirmed.scan.as_ref().expect("scan retained").count,
            count,
        )
        .map_err(PollError::unsettled)
        .and_then(|()| {
            raw.deposits
                .iter()
                .map(|deposit| {
                    if hashes.get(&deposit.block) != Some(&deposit.block_hash) {
                        return Err(PollError::unsettled(anyhow!(
                            "deposit {} is reported in block {} {}, which is not the canonical block there",
                            deposit.id,
                            deposit.block,
                            deposit.block_hash
                        )));
                    }
                    let ordinal = ordinals.entry(deposit.tx_hash).or_default();
                    let receipt = receipt(deposit, *ordinal)?;
                    *ordinal += 1;
                    Ok((receipt, deposit.payer))
                })
                .collect::<Result<Vec<_>, _>>()
        });
        let receipts = match validated {
            Ok(receipts) => receipts,
            Err(cause) => {
                self.confirmed.raw = None;
                return Err(cause);
            }
        };
        self.confirmed.raw = None;
        let scan = self.confirmed.scan.as_mut().expect("scan retained");
        scan.hashes.extend(hashes);
        scan.payers.extend(receipts.iter().map(|(receipt, payer)| (receipt.reference(), *payer)));
        scan.canonical.extend(receipts.into_iter().map(|(receipt, _)| receipt));
        scan.end = Some(window_end);
        scan.count = count;
        self.chain.insert(window_end.number, window_end.hash);
        self.log_range.succeeded();
        Ok(())
    }

    /// Checks the scan against the finalised chain before receipts through `finalised` are released: the
    /// header of every block such a receipt is in, the vault's count at `finalised`, whose hash the release
    /// tag or the scan's own end gives, and any window end stored there. Final blocks never change, so a
    /// read of them names the chain every later one builds on. On a mismatch the scan starts again from the
    /// finalised frontier, whose reconciliation then moves or reverses whatever the final chain does not
    /// hold. Returns whether the scan held.
    /// Error cases:
    /// - stalled: a read fails or lacks a quorum
    pub(super) async fn verify_final(&mut self, from: u64, finalised: BlockRef) -> Result<bool, PollError> {
        let scan = self.confirmed.scan.as_ref().expect("scan retained");
        let releasing =
            || scan.canonical[scan.finalised..].iter().take_while(|receipt| receipt.block <= finalised.number);
        let blocks: BTreeSet<u64> = releasing().map(|receipt| receipt.block).collect();
        let expected = scan.count_at(finalised.number);
        let counted = scan.end == Some(finalised);
        let hashes = self.block_hashes(&blocks).await?;
        let scan = self.confirmed.scan.as_ref().expect("scan retained");
        let moved = scan.canonical[scan.finalised..]
            .iter()
            .take_while(|receipt| receipt.block <= finalised.number)
            .find(|receipt| hashes.get(&receipt.block) != Some(&receipt.block_hash))
            .map(DepositReceipt::reference);
        let count =
            if counted { expected } else { self.vault.deposit_count(finalised).await.map_err(PollError::rpc)? };
        let end_holds = self.chain.get(finalised.number).is_none_or(|hash| hash == finalised.hash);
        if moved.is_none() && count == expected && end_holds {
            return Ok(true);
        }
        info!(
            finalised = finalised.number,
            moved = ?moved,
            count,
            expected,
            end_holds,
            "the finalised chain differs from the scan; reading again from the finalised frontier"
        );
        self.reset_confirmed_chain(from);
        Ok(false)
    }
}
