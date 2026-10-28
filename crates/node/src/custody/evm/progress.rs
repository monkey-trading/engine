//! Bounded, cancellation-safe confirmation reads. Only complete proofs can drive transitions.

use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::B256;
use anyhow::anyhow;
use engine_core::deposits::{CustodyCheckpoint, DepositReceipt};
use engine_core::types::AccountId;

use crate::custody::chain::{LinkedRun, ProvenChain};
use crate::custody::evm::lifecycle::receipt;
use crate::custody::evm::{BlockRef, DepositWatcher, VaultDeposit, VaultReader, check_complete};
use crate::custody::stall::PollError;

#[derive(Default)]
pub(super) struct ConfirmedProgress {
    pub(super) scan: Option<ConfirmedScan>,
    proof: Option<HeaderProof>,
    window: Option<WindowProof>,
    /// The staking mirror's own retained window, separate from the deposit proofs that share
    /// `window`: catch-up survives deposit activity and an advancing release tag.
    staking_window: Option<WindowProof>,
    /// At most 64 binary levels plus two endpoints; an incomplete run has two private candidates.
    pub(super) staking_chain: ProvenChain,
    raw: Option<RawWindow>,
}

struct HeaderProof {
    below: Option<(u64, B256)>,
    first: u64,
    run: LinkedRun,
}

struct WindowProof {
    from: u64,
    to: u64,
    upper: (u64, B256),
    run: LinkedRun,
}

struct RawWindow {
    from: u64,
    to: u64,
    hashes: BTreeMap<u64, B256>,
    deposits: Option<Vec<VaultDeposit>>,
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
}

impl<V: VaultReader> DepositWatcher<V> {
    /// Extends the canonical proof by at most one configured header batch per poll.
    /// The run stays in the watcher across every await, including cancellation.
    /// Error cases:
    /// - quorum reads fail or headers are inconsistent
    pub(super) async fn prove_confirmed(&mut self, from: u64, tagged: BlockRef) -> Result<bool, PollError> {
        if self.chain.get(tagged.number) == Some(tagged.hash) {
            self.confirmed.proof = None;
            return Ok(true);
        }
        if self.chain.top().is_some_and(|(top, _)| top > tagged.number) {
            let Some(hashes) = self.confirmed_window(tagged.number, tagged.number).await? else {
                return Ok(false);
            };
            if hashes[&tagged.number] == tagged.hash {
                return Ok(true);
            }
            self.reset_confirmed_chain(from);
            return Ok(false);
        }
        let below = self.chain.top();
        let first = below.map_or(from, |(number, _)| number + 1);
        if !self.confirmed.proof.as_ref().is_some_and(|proof| proof.below == below && proof.first == first) {
            self.confirmed.proof = Some(HeaderProof { below, first, run: LinkedRun::new(below, first) });
        }
        let next = self.confirmed.proof.as_ref().expect("proof initialized").run.next();
        if next <= tagged.number {
            let end = tagged.number.min(next.saturating_add(self.header_batch.get() - 1));
            let links = self.headers(next, end).await?;
            let window_from = self.confirmed.scan.as_ref().map_or(from, ConfirmedScan::next);
            let window_end = window_from.saturating_add(self.max_block_range);
            let every = self.max_block_range;
            if let Err(cause) = self.confirmed.proof.as_mut().expect("proof retained").run.extend(
                end,
                &links,
                |n| (n + 1).is_multiple_of(every),
                |n| n >= window_from && n < window_end,
            ) {
                self.reset_confirmed_chain(from);
                return if below.is_some_and(|(number, _)| number >= from) {
                    Ok(false)
                } else {
                    Err(PollError::unsettled(cause))
                };
            }
        }
        let run = &self.confirmed.proof.as_ref().expect("proof retained").run;
        if run.next() <= tagged.number {
            return Ok(false);
        }
        if run.last() != Some((tagged.number, tagged.hash)) {
            self.reset_confirmed_chain(from);
            return if below.is_some_and(|(number, _)| number >= from) {
                Ok(false)
            } else {
                Err(PollError::unsettled(anyhow!("confirmation headers did not reach the quorum tag")))
            };
        }
        let proof = self.confirmed.proof.take().expect("complete proof retained");
        self.chain.keep(proof.run);
        self.chain.insert(tagged.number, tagged.hash);
        Ok(true)
    }

    /// Discards all unfinalised evidence after a canonical conflict, retaining only core finality.
    fn reset_confirmed_chain(&mut self, from: u64) {
        self.chain.prune_above(from.checked_sub(1));
        self.confirmed = ConfirmedProgress::default();
    }

    /// Proves a window in at most one header batch, retaining partial work across cancellation.
    /// Error cases:
    /// - headers lack quorum, fail to link, or do not reach the previously proven anchor
    pub(super) async fn confirmed_window(
        &mut self,
        from: u64,
        to: u64,
    ) -> Result<Option<BTreeMap<u64, B256>>, PollError> {
        self.prove_window(from, to, false).await
    }

    /// The end of the staking window whose header proof, begun at `from`, is still in progress.
    pub(super) fn staking_window_end(&self, from: u64) -> Option<u64> {
        self.confirmed.staking_window.as_ref().filter(|proof| proof.from == from).map(|proof| proof.to)
    }

    /// Proves a window for the staking mirror on its own retained slot, so deposit proofs cannot
    /// discard a catch-up in progress. See `confirmed_window`.
    /// Error cases:
    /// - headers lack quorum, fail to link, or do not reach the previously proven anchor
    pub(super) async fn staking_window(
        &mut self,
        from: u64,
        to: u64,
    ) -> Result<Option<BTreeMap<u64, B256>>, PollError> {
        self.prove_window(from, to, true).await
    }

    async fn prove_window(
        &mut self,
        from: u64,
        to: u64,
        staking: bool,
    ) -> Result<Option<BTreeMap<u64, B256>>, PollError> {
        if let Some(hashes) = self.chain.window(from, to).filter(|_| !staking) {
            self.chain.insert(to, hashes[&to]);
            return Ok(Some(hashes));
        }
        let retained = staking
            .then_some(self.confirmed.staking_window.as_ref())
            .flatten()
            .filter(|proof| (proof.from, proof.to) == (from, to))
            .map(|proof| proof.upper);
        let upper = retained
            .or_else(|| {
                staking
                    .then(|| self.confirmed.staking_chain.at_or_above(to))
                    .flatten()
                    .into_iter()
                    .chain(self.chain.at_or_above(to))
                    .min_by_key(|(number, _)| *number)
            })
            .ok_or_else(|| PollError::unsettled(anyhow!("block {to} is above the proven chain")))?;
        if staking {
            let links = self.headers(upper.0, upper.0).await?;
            if links.len() != 1 || links[0].number != upper.0 || links[0].hash != upper.1 {
                self.confirmed.staking_window = None;
                self.confirmed.staking_chain = ProvenChain::default();
                return Err(PollError::unsettled(anyhow!("staking proof anchor changed")));
            }
        }
        if staking && let Some(hashes) = self.confirmed.staking_chain.window(from, to) {
            return Ok(Some(hashes));
        }
        let midpoint = from + (upper.0 - from) / 2;
        let (next, end) = {
            let slot = if staking { &mut self.confirmed.staking_window } else { &mut self.confirmed.window };
            if slot.as_ref().is_none_or(|proof| (proof.from, proof.to, proof.upper) != (from, to, upper)) {
                *slot = Some(WindowProof { from, to, upper, run: LinkedRun::new(None, from) });
            }
            let next = slot.as_ref().expect("window initialized").run.next();
            (next, upper.0.min(next.saturating_add(self.header_batch.get() - 1)))
        };
        // The retained slot is not read across the header fetch; headers cannot touch it, so the
        // run below is the same one initialized above.
        let links = self.headers(next, end).await?;
        let slot = if staking { &mut self.confirmed.staking_window } else { &mut self.confirmed.window };
        let run = &mut slot.as_mut().expect("window retained").run;
        if let Err(cause) = run.extend(
            end,
            &links,
            |number| staking && (number == midpoint || number == upper.0),
            |number| (from..=to).contains(&number),
        ) {
            *slot = None;
            return Err(PollError::unsettled(cause));
        }
        if run.next() <= upper.0 {
            return Ok(None);
        }
        if run.last() != Some(upper) {
            *slot = None;
            return Err(PollError::unsettled(anyhow!("window did not reach its canonical anchor")));
        }
        let proof = slot.take().expect("complete window retained");
        let hashes = proof.run.window(from, to);
        // Midpoints form a depth-first binary hierarchy: O(N log(N/chunk)) header work,
        // at most 66 anchors and one configured chunk of full hashes, independent of deposits.
        let chain = if staking { &mut self.confirmed.staking_chain } else { &mut self.chain };
        chain.clear_window();
        chain.keep(proof.run);
        chain.insert(to, hashes[&to]);
        if staking {
            chain.cap_anchors(66);
        }
        Ok(Some(hashes))
    }

    /// Reuses complete windows only while their endpoint is on the freshly proved chain.
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

    /// Appends at most one complete log window. Logs awaiting their exact-hash count remain
    /// private evidence, so quota errors and cancellation do not repeat completed reads.
    /// Error cases:
    /// - reads fail or canonical headers, complete event ids, counts or economics disagree
    pub(super) async fn advance_confirmed_scan(
        &mut self,
        eligible: u64,
        needed: &BTreeSet<u64>,
    ) -> Result<bool, PollError> {
        let scan = self.confirmed.scan.as_ref().expect("scan initialized");
        let next = scan.next();
        if next > eligible {
            return Ok(true);
        }
        let end = eligible.min(next.saturating_add(self.log_range.get() - 1));
        if self.confirmed.raw.as_ref().is_some_and(|raw| {
            raw.from != next || raw.to > eligible || self.chain.get(raw.to) != Some(raw.hashes[&raw.to])
        }) {
            self.confirmed.raw = None;
        }
        if self.confirmed.raw.is_none() {
            let Some(hashes) = self.confirmed_window(next, end).await? else {
                return Ok(false);
            };
            self.chain.insert(end, hashes[&end]);
            self.confirmed.raw = Some(RawWindow { from: next, to: end, hashes, deposits: None });
        }
        let raw = self.confirmed.raw.as_ref().expect("window retained");
        if raw.deposits.is_none() {
            let deposits = match self.vault.deposits(raw.from, raw.to).await {
                Ok(deposits) => deposits,
                Err(cause) => {
                    self.log_range.failed();
                    self.confirmed.raw = None;
                    return Err(PollError::rpc(cause));
                }
            };
            self.confirmed.raw.as_mut().expect("window retained").deposits = Some(deposits);
        }
        let raw = self.confirmed.raw.as_ref().expect("window retained");
        let count = self
            .vault
            .deposit_count(BlockRef { number: raw.to, hash: raw.hashes[&raw.to] })
            .await
            .map_err(PollError::rpc)?;
        let deposits = raw.deposits.as_ref().expect("logs retained");
        let mut ordinals = BTreeMap::<B256, u64>::new();
        let validated = check_complete(
            deposits,
            raw.from,
            raw.to,
            self.confirmed.scan.as_ref().expect("scan retained").count,
            count,
        )
        .map_err(PollError::unsettled)
        .and_then(|()| {
            deposits
                .iter()
                .map(|deposit| {
                    if raw.hashes.get(&deposit.block) != Some(&deposit.block_hash) {
                        return Err(PollError::unsettled(anyhow!("deposit is outside the proven canonical chain")));
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
        let raw = self.confirmed.raw.take().expect("validated window retained");
        let receipt_blocks: BTreeSet<_> = receipts.iter().map(|(receipt, _)| receipt.block).collect();
        let scan = self.confirmed.scan.as_mut().expect("scan retained");
        scan.hashes.extend(
            raw.hashes
                .into_iter()
                .filter(|(block, _)| *block == raw.to || needed.contains(block) || receipt_blocks.contains(block)),
        );
        let block = BlockRef { number: raw.to, hash: scan.hashes[&raw.to] };
        scan.payers.extend(receipts.iter().map(|(receipt, payer)| (receipt.reference(), *payer)));
        scan.canonical.extend(receipts.into_iter().map(|(receipt, _)| receipt));
        scan.end = Some(block);
        scan.count = count;
        self.chain.insert(block.number, block.hash);
        self.chain.clear_window();
        self.log_range.succeeded();
        Ok(true)
    }
}
