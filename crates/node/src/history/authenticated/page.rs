//! Proven exclusive bounds, consecutive dense ranks and complete payload/lookahead reconstruction.

use anyhow::{Result, ensure};

use crate::history::authenticated::Index;
use crate::history::authenticated::blobs::read_event;
use crate::history::authenticated::lists::{metadata, row};
use crate::history::authenticated::model::{ListId, Metadata, Row, kind_id};
use crate::history::authenticated::overlay::Overlay;
use crate::history::authenticated::state::{State, checked_state};
use crate::history::{Page, PageQuery};
use crate::log::Offset;

impl Index {
    /// Reads one exact existing-API page under one COMPLETE pinned root.
    /// Error cases:
    /// - invalid query, state/metadata/rank/chunk proof failure, malformed payload or scope mismatch
    pub fn page(&self, query: PageQuery) -> Result<Page> {
        query.validate()?;
        let list = ListId { account: query.account, kind: query.kind.as_deref().map(kind_id).transpose()? };
        let _pin = self.store.lease(&self.head.root)?;
        let state = checked_state(&self.store, &self.head)?;
        let mut overlay = Overlay::new(&self.store, &self.head.root);
        let Some(meta) = metadata(&mut overlay, list, &state)? else {
            return Ok(Page { events: Vec::new(), next: None });
        };
        let mut search = Search { overlay, list, meta, state };
        let lo = match query.after {
            Some(after) => search.bound(after, true)?,
            None => search.meta.lo,
        };
        let hi = match query.before {
            Some(before) => search.bound(before, false)?,
            None => search.meta.hi,
        };
        let mut values: Vec<Row> = Vec::with_capacity(query.limit + 1);
        let mut rank = hi;
        while rank > lo && values.len() <= query.limit {
            rank -= 1;
            let value = search.at(rank)?;
            if let Some(newer) = values.last() {
                ensure!(value.cursor < newer.cursor && value.ts <= newer.ts, "unordered history page ranks");
            }
            ensure!(query.after.is_none_or(|after| value.cursor > after), "history rank crosses after bound");
            ensure!(query.before.is_none_or(|before| value.cursor < before), "history rank crosses before bound");
            values.push(value);
        }
        // Lookahead payload must also be complete; a missing piece cannot become false next=None.
        let mut events = values
            .into_iter()
            .map(|value| {
                let event = read_event(&self.store, &self.head.root, &value)?;
                Ok(value.into_event(event))
            })
            .collect::<Result<Vec<_>>>()?;
        let next = if events.len() > query.limit {
            events.truncate(query.limit);
            events.last().map(|event| event.cursor)
        } else {
            None
        };
        Ok(Page { events, next })
    }
}

struct Search {
    overlay: Overlay,
    list: ListId,
    meta: Metadata,
    state: State,
}

impl Search {
    /// Proves one fixed-size header without reading unrelated event bodies.
    /// Error cases:
    /// - missing or inconsistent required rank
    fn at(&mut self, rank: u64) -> Result<Row> {
        row(&mut self.overlay, self.list, &self.meta, rank, &self.state)
    }

    /// Finds the first cursor >= target, or > target when skipping equality, with adjacent witnesses.
    /// Error cases:
    /// - invalid probe, missing rank or inconsistent boundary witnesses
    fn bound(&mut self, target: Offset, skip_equal: bool) -> Result<u64> {
        let precedes = |cursor: Offset| cursor < target || (skip_equal && cursor == target);
        let (mut lo, mut hi) = (self.meta.lo, self.meta.hi);
        while lo < hi {
            let middle = lo + (hi - lo) / 2;
            if precedes(self.at(middle)?.cursor) {
                lo = middle + 1;
            } else {
                hi = middle;
            }
        }
        let left = if lo > self.meta.lo { Some(self.at(lo - 1)?) } else { None };
        let right = if lo < self.meta.hi { Some(self.at(lo)?) } else { None };
        if let Some(left) = &left {
            ensure!(precedes(left.cursor), "history left search witness fails");
        }
        if let Some(right) = &right {
            ensure!(!precedes(right.cursor), "history right search witness fails");
        }
        if let (Some(left), Some(right)) = (left, right) {
            ensure!(left.cursor < right.cursor && left.ts <= right.ts, "unordered history search witnesses");
        }
        Ok(lo)
    }
}
