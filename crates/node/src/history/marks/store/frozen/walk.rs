//! The walk of a frozen copy's rows, in the order the copy is read: each pair's chain checked against the anchors a
//! daily snapshot carries, each row's tag checked under a run key when one is given, and every row folded into the
//! root over them (`Root`). The freeze walks its copy under the key that wrote it to find the root the snapshot
//! carries; a start walks the copy it imports to check the rows against that root, or, for a snapshot that carries
//! none, under its own key.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, bail, ensure};
use engine_core::eip712::B256;
use engine_core::types::MarketId;
use rusqlite::Connection;

use crate::history::marks::Resolution;
use crate::history::marks::store::auth::Pins;
use crate::history::marks::store::chain::verify_row;
use crate::history::marks::store::frozen::root::Root;
use crate::history::marks::store::frozen::{Frozen, FrozenRow};
use crate::history::marks::store::setup::read_epoch;
use crate::history::marks::store::{ChainAnchor, ChainHead, Mac, SCHEMA_VERSION, SnapRow, read_only, store};
use crate::history::store::SchemaRow;

/// One row the walk of a frozen copy passed, in order: its pair, the row as the copy holds it, and the tag of the row
/// below it as the import binds it, none for its pair's floor.
pub(super) struct Passed<'a> {
    pub market: MarketId,
    pub resolution: Resolution,
    pub row: &'a SnapRow,
    pub prev_tag: Option<[u8; 16]>,
}

/// What the walk of one pair's rows in a frozen copy has passed: the pair's anchor, the last row read, its tag as
/// imported, and the pins it met.
struct Walk {
    pair: (MarketId, Resolution),
    anchor: ChainAnchor,
    pins: Pins,
    last: SnapRow,
    imported: [u8; 16],
    pinned: usize,
}

impl Walk {
    /// The walk of `pair`'s chain from its first row, `row`, which must be the floor `anchor` names, with the tag
    /// the import binds it to.
    /// Error cases:
    /// - the row is not the anchored floor, or names a row below it
    fn start(
        pair: (MarketId, Resolution),
        anchor: ChainAnchor,
        pins: Pins,
        row: SnapRow,
    ) -> anyhow::Result<(Self, Option<[u8; 16]>)> {
        ensure!(
            (row.bucket, row.tag) == (anchor.floor.bucket, anchor.floor.tag),
            "the {pair:?} chain's floor is not the one the daily snapshot anchors"
        );
        ensure!(row.prev.is_none() && row.prev_tag.is_none(), "the {pair:?} chain's floor names a row below it");
        let walk = Self { pair, anchor, pins, last: row, imported: [0; 16], pinned: 0 };
        Ok((walk.pinning()?, None))
    }

    /// The walk on to `row`, the next row of its pair, which must name the last one and bind the tag it carries,
    /// unless that one is pinned, with the tag the import binds it to: the last one's as imported.
    /// Error cases:
    /// - the row's link names another bucket, or binds another version of the row below it
    fn step(self, row: SnapRow) -> anyhow::Result<(Self, Option<[u8; 16]>)> {
        let pair = self.pair;
        ensure!(row.prev == Some(self.last.bucket), "the {pair:?} chain is broken at {}", row.bucket);
        ensure!(
            self.pins.contains_key(&self.last.bucket) || row.prev_tag == Some(self.last.tag),
            "the {pair:?} bucket at {} binds another version of the one below it",
            row.bucket
        );
        let imported = Some(self.imported);
        Ok((Self { last: row, ..self }.pinning()?, imported))
    }

    /// This walk, its last row counted if it is pinned.
    /// Error cases:
    /// - the row is pinned at another tag
    fn pinning(mut self) -> anyhow::Result<Self> {
        if let Some(pinned) = self.pins.get(&self.last.bucket) {
            ensure!(
                *pinned == self.last.tag,
                "the {:?} bucket at {} is not its pinned version",
                self.pair,
                self.last.bucket
            );
            self.pinned += 1;
        }
        Ok(self)
    }

    /// Ends the walk of its pair at its last row, which must be the head its anchor names, every pin met.
    /// Error cases:
    /// - the last row is not the anchored head, or a pinned row was not met
    fn end(self) -> anyhow::Result<()> {
        let pair = self.pair;
        ensure!(
            (self.last.bucket, self.last.tag) == (self.anchor.head.bucket, self.anchor.head.tag),
            "the {pair:?} chain's head is not the one the daily snapshot anchors"
        );
        ensure!(self.pinned == self.pins.len(), "the {pair:?} chain lacks a row the daily snapshot pins");
        Ok(())
    }
}

/// Opens the frozen copy at `copy` read-only, once it holds this build's schema, `schema`, and the run nonce `frozen`
/// names.
/// Error cases:
/// - the copy cannot be read, or holds another schema or another run nonce
pub(super) fn open_copy(copy: &Path, schema: &[SchemaRow], frozen: &Frozen) -> anyhow::Result<Connection> {
    let from = read_only(copy)?;
    let version: i64 = from.pragma_query_value(None, "user_version", |row| row.get(0)).context("reading it")?;
    ensure!(version == SCHEMA_VERSION, "it holds marks schema {version}, not {SCHEMA_VERSION}");
    store::check_schema(&from, schema, copy)?;
    ensure!(read_epoch(&from)? == frozen.epoch, "its run nonce is not the one the daily snapshot names");
    Ok(from)
}

/// Reads the rows of the frozen copy `from` in key order and passes each to `take`, which gives the tag the import
/// binds it to: each row's tag authenticated under `tags`, when given, and each pair's chain the one `frozen` anchors
/// (`Walk`), with no pair more or fewer. Gives the pairs walked and the root over every row read (`Root`).
/// Error cases:
/// - a row does not read or is out of order, a tag fails `tags`, or a pair's chain is not the one `frozen` anchors
/// - `frozen` names a resolution this build does not keep or names a pair twice, or a pair it anchors holds no row
/// - `take` fails
pub(super) fn walk(
    from: &Connection,
    frozen: &Frozen,
    tags: Option<&Mac>,
    mut take: impl FnMut(Passed<'_>) -> anyhow::Result<[u8; 16]>,
) -> anyhow::Result<(BTreeSet<(MarketId, Resolution)>, B256)> {
    let mut anchors = anchored(frozen)?;
    let mut read = from
        .prepare(
            "SELECT market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag, prev_tag
             FROM marks ORDER BY market, resolution, bucket",
        )
        .context("reading the frozen marks")?;
    let mut rows = read.query([]).context("reading the frozen marks")?;
    let (mut walk, mut pairs, mut root): (Option<Walk>, BTreeSet<_>, Root) = (None, BTreeSet::new(), Root::default());
    while let Some(row) = rows.next().context("reading the frozen marks")? {
        let (market, resolution) = (row.get::<_, MarketId>(0)?, row.get::<_, i64>(1)?);
        let resolution = u8::try_from(resolution).ok().and_then(Resolution::from_tag).context("a resolution")?;
        let pair = (market, resolution);
        let row = SnapRow::from_row(row, 2).context("reading a frozen mark")?;
        if let Some(mac) = tags {
            verify_row(mac, market, resolution, &row)?;
        }
        root.add(market, resolution, &row)?;
        let below = match walk.take() {
            Some(walking) if walking.pair == pair => Some(walking),
            passed => {
                passed.map(Walk::end).transpose()?;
                None
            }
        };
        let (next, prev_tag) = match below {
            Some(below) => below.step(row)?,
            None => {
                let (anchor, pins) = anchors.remove(&pair).with_context(|| format!("no anchor names {pair:?}"))?;
                ensure!(pairs.insert(pair), "the {pair:?} rows are not in order");
                Walk::start(pair, anchor, pins, row)?
            }
        };
        let imported = take(Passed { market, resolution, row: &next.last, prev_tag })?;
        walk = Some(Walk { imported, ..next });
    }
    walk.map(Walk::end).transpose()?;
    if let Some(pair) = anchors.keys().next() {
        bail!("the {pair:?} chain the daily snapshot anchors holds no row");
    }
    Ok((pairs, root.finish()))
}

/// Each pair's anchor and pins `frozen` names.
/// Error cases:
/// - a pair names a resolution this build does not keep, or is named twice
fn anchored(frozen: &Frozen) -> anyhow::Result<BTreeMap<(MarketId, Resolution), (ChainAnchor, Pins)>> {
    let mut anchors = BTreeMap::new();
    for pair in &frozen.pairs {
        let resolution = Resolution::from_tag(pair.resolution).context("a resolution this build does not keep")?;
        let head = |row: FrozenRow| ChainHead { bucket: row.bucket, tag: row.tag };
        let anchor = ChainAnchor { floor: head(pair.floor), head: head(pair.head) };
        let pins = pair.pins.iter().map(|row| (row.bucket, row.tag)).collect();
        ensure!(anchors.insert((pair.market, resolution), (anchor, pins)).is_none(), "a pair is named twice");
    }
    Ok(anchors)
}
