//! Row authentication for the marks cache: the coverage a backfill recorded, the HMAC that
//! binds every stored row to the enclave key that wrote it, and the chained-index head that
//! anchors each market/resolution chain at recovery re-derivation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use engine_core::types::{MarketId, Millis};

use crate::history::marks::Resolution;

/// What one backfill recorded as its coverage: the depths the config asked for and the retentions
/// that pruned the seed. A later config that asks for more re-runs the backfill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Coverage {
    pub depths: (Millis, Millis, Millis),
    pub retentions: (Millis, Millis, Millis, Option<Millis>),
}

/// One chain's committed end: a bucket's window start and its authentication tag, as anchored in
/// memory and advanced by every write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChainHead {
    pub bucket: Millis,
    pub tag: [u8; 16],
}

/// One pair's committed chain anchor: its floor and its head, both as written by this node's
/// writes. A file whose floor or head differs from the anchor is a rollback or a deletion the
/// reader refuses; what changed between them is bound by the per-row links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChainAnchor {
    pub floor: ChainHead,
    pub head: ChainHead,
}

/// The rows of one pair a write rewrote while a row sat above them, by bucket and tag as committed:
/// a successor binds its predecessor's tag as written with it, so these are the rows whose tag has
/// since moved, and the committed tag is the only version a read accepts for them.
pub(crate) type Interior = BTreeMap<Millis, [u8; 16]>;

/// The buckets a write rewrote in place, per pair, whose tags `anchor_updates` reads at commit.
pub(crate) type Rewrites = BTreeMap<(MarketId, Resolution), BTreeSet<Millis>>;

/// What one write commits for a pair: its anchor, the tags of the interior rows it rewrote, and
/// whether it re-bound the whole pair, which clears the interior set.
#[derive(Debug, Clone)]
pub(crate) struct PairCommit {
    pub anchor: ChainAnchor,
    pub rewritten: Interior,
    pub rebound: bool,
}

/// The committed state of every (market, resolution) pair, established at recovery re-derivation
/// and advanced by every write, atomically with its commit. The store and its readers share one:
/// the writer merges in the pairs a transaction touched, a reader checks one pair's floor and head,
/// and the interior rows pinned inside them, before serving a page. Derefs to the anchors.
#[derive(Debug, Clone, Default)]
pub(crate) struct ChainAnchors {
    anchors: BTreeMap<(MarketId, Resolution), ChainAnchor>,
    interior: BTreeMap<(MarketId, Resolution), Arc<Interior>>,
}

impl std::ops::Deref for ChainAnchors {
    type Target = BTreeMap<(MarketId, Resolution), ChainAnchor>;
    fn deref(&self) -> &Self::Target {
        &self.anchors
    }
}

impl std::ops::DerefMut for ChainAnchors {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.anchors
    }
}

impl<'a> IntoIterator for &'a ChainAnchors {
    type Item = (&'a (MarketId, Resolution), &'a ChainAnchor);
    type IntoIter = std::collections::btree_map::Iter<'a, (MarketId, Resolution), ChainAnchor>;
    fn into_iter(self) -> Self::IntoIter {
        self.anchors.iter()
    }
}

impl ChainAnchors {
    /// The interior rows pinned for `pair`: shared, so a reader holds the set it checked a page
    /// against while the writer moves on.
    pub(crate) fn interior(&self, pair: (MarketId, Resolution)) -> Arc<Interior> {
        self.interior.get(&pair).cloned().unwrap_or_default()
    }

    /// Merges a transaction's commits: `Some` replaces the pair's anchor and pins what it
    /// rewrote, `None` removes a pair whose every bucket the transaction deleted. Pins below the
    /// floor go; the set is copied only when it changes.
    pub(crate) fn apply(&mut self, updates: crate::history::marks::store::chain::ChainUpdates) {
        for (pair, commit) in updates {
            let Some(commit) = commit else {
                self.anchors.remove(&pair);
                self.interior.remove(&pair);
                continue;
            };
            let floor = commit.anchor.floor.bucket;
            self.anchors.insert(pair, commit.anchor);
            let pinned = self.interior.entry(pair).or_default();
            let expired = pinned.first_key_value().is_some_and(|(bucket, _)| *bucket < floor);
            if commit.rebound || expired || !commit.rewritten.is_empty() {
                let pinned = Arc::make_mut(pinned);
                if commit.rebound {
                    pinned.clear();
                }
                while pinned.first_key_value().is_some_and(|(bucket, _)| *bucket < floor) {
                    pinned.pop_first();
                }
                pinned.extend(commit.rewritten);
            }
        }
    }
}

/// The row fields a tag covers: the OHLC, provenance, the chained index's predecessor bucket, and
/// the tag that predecessor carried when this row was written, as stored. `prev` and `prev_tag`
/// are `None` for the chain's floor row.
pub(crate) struct Row<'a> {
    pub open: &'a str,
    pub high: &'a str,
    pub low: &'a str,
    pub close: &'a str,
    pub provenance: &'a str,
    pub prev: Option<Millis>,
    pub prev_tag: Option<[u8; 16]>,
}

/// The authentication tag of one row: the first half of an HMAC of every stored field under the
/// enclave key's marks derivation, so a row the host rewrote fails verification.
pub(crate) fn tag(
    mac: &hmac::Hmac<sha2::Sha256>,
    market: MarketId,
    resolution: Resolution,
    bucket: Millis,
    row: &Row,
) -> [u8; 16] {
    use hmac::Mac;
    let mut mac = mac.clone();
    for number in [i64::from(market), resolution as i64, bucket] {
        mac.update(&number.to_be_bytes());
        mac.update(&[0]);
    }
    for text in [row.open, row.high, row.low, row.close, row.provenance] {
        mac.update(text.as_bytes());
        mac.update(&[0]);
    }
    match row.prev {
        Some(prev) => {
            mac.update(&[1]);
            mac.update(&prev.to_be_bytes());
        }
        None => mac.update(&[0]),
    }
    match row.prev_tag {
        Some(prev_tag) => {
            mac.update(&[1]);
            mac.update(&prev_tag);
        }
        None => mac.update(&[0]),
    }
    mac.finalize().into_bytes()[..16].try_into().expect("16 bytes from a 32-byte digest")
}
