//! Blocks proven to lead to the block the finality tag reported: every header between two kept
//! anchors was read and each named the one below it as its parent, up to the tagged block's hash.
//! Block hashes are commitments, so the proof holds however the endpoint routes its reads. Only
//! sparse anchors are kept, plus every hash of the next read window, so memory grows with the
//! backlog over the block range, not with it. Headers read by a proof that has not yet reached its
//! end are a separate, unproven run, which only a later attempt at the same proof reads.

use std::collections::BTreeMap;

use alloy::primitives::B256;
use anyhow::bail;

/// A block's hash and its parent's, as a header reports them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockLink {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
}

/// Proven hashes at sparse heights, the top being the tagged block last proven, and every hash
/// read for the blocks the watcher reads next.
#[derive(Default)]
pub(super) struct ProvenChain {
    anchors: BTreeMap<u64, B256>,
    window: BTreeMap<u64, B256>,
    /// The run of an interrupted proof, kept so the next attempt resumes after it.
    interrupted: Option<LinkedRun>,
    #[cfg(test)]
    window_peak: usize,
}

/// Headers a proof has linked upward, each the parent of the next, that have not yet reached the
/// block the proof must end on. None of them is proven until the run does.
pub(super) struct LinkedRun {
    /// The block below the first header, when known, and the first block.
    start: (Option<(u64, B256)>, u64),
    /// The last block linked.
    last: Option<(u64, B256)>,
    anchors: BTreeMap<u64, B256>,
    window: BTreeMap<u64, B256>,
}

impl ProvenChain {
    /// The proven hash at `number`, when it is kept.
    pub(super) fn get(&self, number: u64) -> Option<B256> {
        self.anchors.get(&number).copied()
    }

    /// How many anchors are kept.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.anchors.len()
    }

    /// The highest proven block.
    pub(super) fn top(&self) -> Option<(u64, B256)> {
        self.anchors.last_key_value().map(|(number, hash)| (*number, *hash))
    }

    /// The lowest kept anchor at or above `number`.
    pub(super) fn at_or_above(&self, number: u64) -> Option<(u64, B256)> {
        self.anchors.range(number..).next().map(|(number, hash)| (*number, *hash))
    }

    /// Keeps `hash` as proven for block `number`.
    pub(super) fn insert(&mut self, number: u64, hash: B256) {
        self.anchors.insert(number, hash);
    }

    /// The interrupted run that starts as `LinkedRun::new(below, first)` would, or that new run.
    pub(super) fn resume(&mut self, below: Option<(u64, B256)>, first: u64) -> LinkedRun {
        self.interrupted.take_if(|run| run.start == (below, first)).unwrap_or_else(|| LinkedRun::new(below, first))
    }

    /// Keeps the run of a proof a failed read interrupted, for the next attempt to resume.
    pub(super) fn interrupt(&mut self, run: LinkedRun) {
        self.interrupted = Some(run);
    }

    /// Keeps the anchors and window hashes of `run`, which has reached a proven or tagged block,
    /// as proven.
    pub(super) fn keep(&mut self, run: LinkedRun) {
        self.anchors.extend(run.anchors);
        self.window.extend(run.window);
        #[cfg(test)]
        {
            self.window_peak = self.window_peak.max(self.window.len());
        }
    }

    /// Discards consumed full headers, preserving canonical anchors and interrupted proofs.
    pub(super) fn clear_window(&mut self) {
        self.window.clear();
    }

    /// The largest retained full-header window during this proof.
    #[cfg(test)]
    pub(super) fn window_peak(&self) -> usize {
        self.window_peak
    }

    /// The window's hashes for blocks `from..=to`, when it holds every one of them.
    pub(super) fn window(&self, from: u64, to: u64) -> Option<BTreeMap<u64, B256>> {
        let hashes: BTreeMap<u64, B256> = self.window.range(from..=to).map(|(n, h)| (*n, *h)).collect();
        (hashes.len() as u64 == to - from + 1).then_some(hashes)
    }

    /// Forgets everything below `number`.
    pub(super) fn prune_below(&mut self, number: u64) {
        self.anchors = self.anchors.split_off(&number);
        self.window = self.window.split_off(&number);
    }

    /// Forgets everything above `number`, or everything for `None`, and any interrupted run.
    pub(super) fn prune_above(&mut self, number: Option<u64>) {
        self.interrupted = None;
        match number {
            Some(number) => {
                self.anchors.split_off(&(number + 1));
                self.window.split_off(&(number + 1));
            }
            None => {
                self.anchors.clear();
                self.window.clear();
            }
        }
    }
}

impl LinkedRun {
    /// A run whose first header is block `first`, a child of `below` when that is known.
    pub(super) fn new(below: Option<(u64, B256)>, first: u64) -> Self {
        LinkedRun { start: (below, first), last: None, anchors: BTreeMap::new(), window: BTreeMap::new() }
    }

    /// The last block linked, or the block below the run before any is.
    pub(super) fn last(&self) -> Option<(u64, B256)> {
        self.last.or(self.start.0)
    }

    /// The next block to link.
    pub(super) fn next(&self) -> u64 {
        self.last.map_or(self.start.1, |(number, _)| number + 1)
    }

    /// Links `links`, blocks `next()..=to`, onto the run, keeping the hashes `anchor` names as
    /// anchors and those `window` names for the window.
    /// Error cases:
    /// - the headers do not link on from the run (see `link`)
    pub(super) fn extend(
        &mut self,
        to: u64,
        links: &[BlockLink],
        anchor: impl Fn(u64) -> bool,
        window: impl Fn(u64) -> bool,
    ) -> anyhow::Result<()> {
        self.last = Some(link(self.last(), self.next(), to, links)?);
        for l in links {
            if anchor(l.number) {
                self.anchors.insert(l.number, l.hash);
            }
            if window(l.number) {
                self.window.insert(l.number, l.hash);
            }
        }
        Ok(())
    }

    /// The run's window hashes for blocks `from..=to`.
    pub(super) fn window(&self, from: u64, to: u64) -> BTreeMap<u64, B256> {
        self.window.range(from..=to).map(|(number, hash)| (*number, *hash)).collect()
    }
}

/// Checks `links` are blocks `from..=to` in order, each the parent of the next, and the first a
/// child of `below` when that is known; returns the last.
/// Error cases:
/// - a block is missing or out of order, or a parent hash is not the hash of the block below
fn link(below: Option<(u64, B256)>, from: u64, to: u64, links: &[BlockLink]) -> anyhow::Result<(u64, B256)> {
    if links.len() as u64 != to - from + 1 {
        bail!("asked for headers {from}..={to}, got {}", links.len());
    }
    let mut below = below;
    for (number, link) in (from..=to).zip(links) {
        if link.number != number {
            bail!("asked for block {number}, got block {}", link.number);
        }
        if let Some((below_number, below_hash)) = below
            && (below_number + 1 != number || link.parent_hash != below_hash)
        {
            bail!(
                "block {number} {} names parent {}, not block {below_number} {below_hash}",
                link.hash,
                link.parent_hash
            );
        }
        below = Some((number, link.hash));
    }
    Ok(below.expect("at least one link"))
}
