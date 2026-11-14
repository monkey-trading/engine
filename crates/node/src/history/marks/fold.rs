//! A member's marks while it follows another holder (#593). The member holds no enclave key then,
//! so it cannot write the cache's authenticated rows: its replica's mark changes are folded instead
//! into per-resolution OHLC buckets, as the cache keeps them, and merged into the cache once the
//! member's node starts from the replica. Each resolution keeps only what `[history.marks]` retains
//! for it, pruned as the marks arrive, so a member that follows for weeks keeps what the cache would:
//! about 100 bytes per market and second at 1s (48 h by default, some 17 MB per market), and as much
//! per bucket at each coarser resolution. No raw sample is kept.

use std::collections::BTreeMap;

use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};

use super::{Resolution, Retentions};

/// One folded bucket: its market, resolution and window start, and its open, high, low and close.
pub(crate) type Folded = (MarketId, Resolution, Millis, [Fixed; 4]);

/// The folded buckets of each resolution, keyed by window start and market.
#[derive(Debug)]
pub(crate) struct MarkFold {
    retentions: Retentions,
    buckets: [BTreeMap<(Millis, MarketId), [Fixed; 4]>; 4],
    newest: Option<Millis>,
}

impl Default for MarkFold {
    /// An empty fold under the default retentions.
    fn default() -> Self {
        Self::new(Retentions::defaults())
    }
}

impl MarkFold {
    /// An empty fold keeping each resolution for its retention in `retentions`.
    pub(crate) fn new(retentions: Retentions) -> Self {
        Self { retentions, buckets: Default::default(), newest: None }
    }

    /// Folds `mark`, `market`'s mark at sequenced time `ts`, into its bucket at every resolution, as
    /// the cache merges a sample: open kept, high and low widened, close moved. Then drops the
    /// buckets each resolution no longer retains, counted back from the newest mark, as the cache
    /// prunes. Marks arrive in sequenced order.
    pub(crate) fn sample(&mut self, market: MarketId, ts: Millis, mark: Fixed) {
        let newest = self.newest.map_or(ts, |newest| newest.max(ts));
        self.newest = Some(newest);
        for resolution in Resolution::ALL {
            let buckets = &mut self.buckets[resolution as usize];
            buckets
                .entry((resolution.bucket(ts), market))
                .and_modify(|[_, high, low, close]| {
                    *high = (*high).max(mark);
                    *low = (*low).min(mark);
                    *close = mark;
                })
                .or_insert([mark; 4]);
            if let Some(retention) = self.retentions.of_resolution(resolution) {
                let horizon = newest.saturating_sub(retention);
                while buckets.first_key_value().is_some_and(|((bucket, _), _)| *bucket < horizon) {
                    buckets.pop_first();
                }
            }
        }
    }

    /// Every folded bucket, each resolution's oldest first, and the newest mark's time; `None` when
    /// nothing was folded.
    pub(crate) fn into_buckets(self) -> Option<(Vec<Folded>, Millis)> {
        let newest = self.newest?;
        let buckets = Resolution::ALL
            .into_iter()
            .zip(self.buckets)
            .flat_map(|(resolution, buckets)| {
                buckets.into_iter().map(move |((bucket, market), marks)| (market, resolution, bucket, marks))
            })
            .collect();
        Some((buckets, newest))
    }
}

#[cfg(test)]
mod tests;
