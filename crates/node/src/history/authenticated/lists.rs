//! Verified dense-list allocation, append, and chronological head removal.

use anyhow::{Context, Result, ensure};

use crate::history::authenticated::model::{ListId, Metadata, Position, Row};
use crate::history::authenticated::overlay::Overlay;
use crate::history::authenticated::state::State;

/// Proves metadata membership or absence and validates a present dense interval.
/// Error cases:
/// - proof/decoding failure, empty interval, or unallocated generation
pub(super) fn metadata(overlay: &mut Overlay, list: ListId, state: &State) -> Result<Option<Metadata>> {
    let meta: Option<Metadata> = overlay.read(&list.key())?;
    if let Some(meta) = &meta {
        meta.validate(state)?;
    }
    Ok(meta)
}

/// Reads a required ordinal; missing interior ranks can never shorten a successful result.
/// Error cases:
/// - out-of-range rank, absent/unproved/malformed value, or row identity mismatch
pub(super) fn row(overlay: &mut Overlay, list: ListId, meta: &Metadata, rank: u64, state: &State) -> Result<Row> {
    ensure!(meta.lo <= rank && rank < meta.hi, "history rank outside dense interval");
    let row: Row = overlay.read(&meta.position(rank).key())?.context("missing required history rank")?;
    row.validate(list, state)?;
    Ok(row)
}

/// Appends to a verified list or allocates a fresh generation after proven metadata absence.
/// Error cases:
/// - old-value proof failure, non-increasing cursor/time, occupied new rank, or counter overflow
pub(super) fn append(overlay: &mut Overlay, list: ListId, value: &Row, state: &mut State) -> Result<Position> {
    value.validate(list, state)?;
    let mut meta = match metadata(overlay, list, state)? {
        Some(meta) => {
            let tail = row(overlay, list, &meta, meta.hi - 1, state)?;
            ensure!(tail.cursor < value.cursor && tail.ts <= value.ts, "unordered history append");
            meta
        }
        None => {
            let generation = state.next_generation;
            state.next_generation = generation.checked_add(1).context("history generation overflow")?;
            Metadata { generation, lo: 0, hi: 0 }
        }
    };
    let position = meta.position(meta.hi);
    ensure!(overlay.read::<Row>(&position.key())?.is_none(), "new history rank is occupied");
    meta.hi = meta.hi.checked_add(1).context("history rank overflow")?;
    overlay.put(position.key(), value)?;
    overlay.put(list.key(), &meta)?;
    Ok(position)
}

/// Removes exactly the oldest rank referenced by a proved chronological queue item.
/// Error cases:
/// - missing/mismatched metadata, generation, head rank, or event payload
pub(super) fn pop(
    overlay: &mut Overlay,
    list: ListId,
    position: Position,
    expected: &Row,
    state: &State,
) -> Result<()> {
    let mut meta = metadata(overlay, list, state)?.context("queue references missing history list")?;
    ensure!(meta.generation == position.generation && meta.lo == position.rank, "queue does not reference list head");
    ensure!(row(overlay, list, &meta, position.rank, state)? == *expected, "queue/list history payload mismatch");
    overlay.delete(position.key())?;
    meta.lo = meta.lo.checked_add(1).context("history rank overflow")?;
    if meta.lo == meta.hi {
        overlay.delete(list.key())?;
    } else {
        overlay.put(list.key(), &meta)?;
    }
    Ok(())
}
