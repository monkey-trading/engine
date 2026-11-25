//! Frozen copies of the marks cache at a daily snapshot's checkpoint. The marks task writes one once the cache
//! holds the marks of every command the checkpoint covers (`Store::freeze`): a copy of the file beside it
//! (`VACUUM INTO`), named for the checkpoint's offset, of which the newest `KEPT` stay, and the cache's committed
//! chain anchors as they stand then (`Frozen`), which the snapshot carries under its signature. A start from the
//! snapshot takes the copy in place of the cache (`Store::import_frozen`) only once every pair's chain in it is
//! the one those anchors name, so a copy the host rolled back, or altered in any row, is refused whole.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use engine_core::types::{MarketId, Millis};
use rusqlite::{TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::history::marks::Resolution;
use crate::history::marks::store::auth::{Epoch, Pins, run_mac};
use crate::history::marks::store::chain::{anchor_updates, verify_row};
use crate::history::marks::store::setup::read_epoch;
use crate::history::marks::store::{ChainAnchor, ChainHead, SCHEMA_VERSION, SnapRow, Store, read_only, store, tag};
use crate::log::Offset;

/// How many frozen copies stay beside the cache: a start takes the newest snapshot it can, and the copies of the
/// days before it serve a start that passes the newest over.
pub(crate) const KEPT: usize = 3;
/// What a frozen copy's name starts with; its checkpoint's offset follows, as a daily snapshot's name gives it.
const PREFIX: &str = "marks-frozen-";

/// The cache's committed chain anchors as a frozen copy holds the cache: the copy's run nonce and each pair's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Frozen {
    #[serde(with = "hex16")]
    pub epoch: Epoch,
    pub pairs: Vec<FrozenPair>,
}

/// One pair's committed anchor: its floor and head, and the rows pinned at their committed tags.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FrozenPair {
    pub market: MarketId,
    /// The resolution's storage tag.
    pub resolution: u8,
    pub floor: FrozenRow,
    pub head: FrozenRow,
    pub pins: Vec<FrozenRow>,
}

/// A row's bucket and authentication tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FrozenRow {
    pub bucket: Millis,
    #[serde(with = "hex16")]
    pub tag: [u8; 16],
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

impl Store {
    /// Writes a frozen copy of the cache beside it for the checkpoint at `offset`, synced, removing the copies past
    /// the newest `KEPT`, and gives the committed anchors it holds. The cache's writes all run on this store, so
    /// nothing commits between the copy and the anchors read after it.
    /// Error cases:
    /// - the copy cannot be written, synced or renamed into place, or its directory listed
    pub(crate) fn freeze(&self, offset: Offset) -> anyhow::Result<Frozen> {
        let dir = self.path.parent().context("the marks cache has no directory")?;
        let path = frozen_path(dir, offset);
        let staged = path.with_extension("tmp");
        store::discard(&staged)?;
        let target = staged.to_str().context("the frozen copy's path is not UTF-8")?;
        self.conn.execute("VACUUM INTO ?1", [target]).context("copying the marks cache")?;
        File::open(&staged).and_then(|file| file.sync_all()).context("syncing the frozen marks cache")?;
        fs::rename(&staged, &path).with_context(|| format!("publishing {}", path.display()))?;
        File::open(dir).and_then(|opened| opened.sync_all()).with_context(|| format!("syncing {}", dir.display()))?;
        prune(dir)?;
        let epoch = read_epoch(&self.conn)?;
        let anchors = self.prior_anchors();
        let pairs = anchors
            .iter()
            .map(|(&(market, resolution), anchor)| {
                let row = |head: ChainHead| FrozenRow { bucket: head.bucket, tag: head.tag };
                let pins = anchors.pins((market, resolution));
                let pins = pins.iter().map(|(&bucket, &tag)| FrozenRow { bucket, tag }).collect();
                FrozenPair {
                    market,
                    resolution: resolution as u8,
                    floor: row(anchor.floor),
                    head: row(anchor.head),
                    pins,
                }
            })
            .collect();
        Ok(Frozen { epoch, pairs })
    }

    /// Imports the frozen copy at `copy` into this cache, which holds nothing yet, once its every pair's chain is
    /// the one `frozen` names: each row authenticates under the copy's run nonce, which is `frozen`'s, names the row
    /// below it and binds the tag that row carries, unless that row is pinned, every pinned row carries its pin, and
    /// each pair's floor and head are its anchor's, with no pair more or fewer. Each row is re-tagged under this
    /// cache's run key as it is taken, as a prior cache is (`import_prior`), with the backfill markers. One
    /// transaction, which a refusal rolls back; the copy is opened read-only and never modified.
    /// Error cases:
    /// - the copy cannot be read, holds another schema or another run nonce, or a row or pair fails the check
    /// - the import cannot be written
    pub(crate) fn import_frozen(&mut self, copy: &Path, frozen: &Frozen) -> anyhow::Result<usize> {
        let from = read_only(copy)?;
        let version: i64 = from.pragma_query_value(None, "user_version", |row| row.get(0)).context("reading it")?;
        ensure!(version == SCHEMA_VERSION, "it holds marks schema {version}, not {SCHEMA_VERSION}");
        store::check_schema(&from, &self.schema, copy)?;
        let epoch = read_epoch(&from)?;
        ensure!(epoch == frozen.epoch, "its run nonce is not the one the daily snapshot names");
        let (copy_mac, mut anchors) = (run_mac(&self.mac_key, &epoch), anchored(frozen)?);
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate).context("starting the import")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        let held: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM marks)", [], |row| row.get(0)).context("reading")?;
        ensure!(!held, "the cache it would go into already holds marks");
        let mut insert = tx
            .prepare(
                "INSERT INTO marks (market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag, prev_tag)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )
            .context("preparing the frozen marks import")?;
        let mut read = from
            .prepare(
                "SELECT market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag, prev_tag
                 FROM marks ORDER BY market, resolution, bucket",
            )
            .context("reading the frozen marks")?;
        let mut rows = read.query([]).context("reading the frozen marks")?;
        let (mut walk, mut pairs, mut imported): (Option<Walk>, BTreeSet<_>, usize) = (None, BTreeSet::new(), 0);
        while let Some(row) = rows.next().context("reading the frozen marks")? {
            let (market, resolution) = (row.get::<_, MarketId>(0)?, row.get::<_, i64>(1)?);
            let resolution = u8::try_from(resolution).ok().and_then(Resolution::from_tag).context("a resolution")?;
            let pair = (market, resolution);
            let row = SnapRow::from_row(row, 2).context("reading a frozen mark")?;
            verify_row(&copy_mac, market, resolution, &row)?;
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
            let last = &next.last;
            let fields = crate::history::marks::store::Row { prev_tag, ..last.fields() };
            let retagged = tag(&self.mac, market, resolution, last.bucket, &fields);
            insert
                .execute(params![
                    i64::from(market),
                    resolution as i64,
                    last.bucket,
                    last.open,
                    last.high,
                    last.low,
                    last.close,
                    last.provenance,
                    last.prev,
                    retagged,
                    prev_tag
                ])
                .with_context(|| format!("importing the frozen {market} {resolution:?} bucket at {}", last.bucket))?;
            imported += 1;
            walk = Some(Walk { imported: retagged, ..next });
        }
        walk.map(Walk::end).transpose()?;
        if let Some(pair) = anchors.keys().next() {
            bail!("the {pair:?} chain the daily snapshot anchors holds no row");
        }
        drop(insert);
        copy_markers(&from, &tx)?;
        let updates = anchor_updates(&tx, &pairs, &Default::default(), false)?;
        let mut committed = self.anchors.lock().expect("the marks chain anchors are never poisoned");
        tx.commit().context("committing the frozen marks import")?;
        committed.apply(updates);
        Ok(imported)
    }
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

/// Copies the backfill markers of the copy `from` into the import `tx`.
/// Error cases:
/// - the markers cannot be read or written
fn copy_markers(from: &rusqlite::Connection, tx: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    let columns = "market, venue, symbol, at, depth_1m, depth_1h, depth_1d, retention_1s, retention_1m, retention_1h, retention_1d";
    let mut read = from.prepare(&format!("SELECT {columns} FROM backfilled")).context("reading the markers")?;
    let mut insert = tx
        .prepare(&format!("INSERT INTO backfilled ({columns}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"))
        .context("preparing the marker import")?;
    let mut rows = read.query([]).context("reading the markers")?;
    while let Some(row) = rows.next().context("reading the markers")? {
        let values: Vec<rusqlite::types::Value> =
            (0..11).map(|column| row.get(column)).collect::<Result<_, _>>().context("reading a marker")?;
        insert.execute(rusqlite::params_from_iter(values)).context("importing a backfill marker")?;
    }
    Ok(())
}

/// Where the frozen copy for the checkpoint at `offset` is kept in `dir`, the cache's directory.
pub(crate) fn frozen_path(dir: &Path, offset: Offset) -> PathBuf {
    dir.join(format!("{PREFIX}{:020}-{:010}.sqlite", offset.seq, offset.index))
}

/// Removes the frozen copies in `dir` past the newest `KEPT`, and any a crash left staged.
/// Error cases:
/// - the directory cannot be listed
fn prune(dir: &Path) -> anyhow::Result<()> {
    let mut copies = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let name = entry.with_context(|| format!("listing {}", dir.display()))?.file_name();
        let Some(name) = name.to_str().filter(|name| name.starts_with(PREFIX)) else { continue };
        match name.strip_suffix(".tmp") {
            Some(_) => copies.push((true, dir.join(name))),
            None if name.ends_with(".sqlite") => copies.push((false, dir.join(name))),
            None => {}
        }
    }
    // Names order as their offsets do.
    copies.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    let held = copies.iter().filter(|(staged, _)| !staged).count();
    let stale = copies.iter().filter(|(staged, _)| !staged).skip(KEPT).map(|(_, path)| path);
    for path in copies.iter().filter(|(staged, _)| *staged).map(|(_, path)| path).chain(stale) {
        if let Err(e) = store::discard(path) {
            warn!(held, "a frozen marks cache could not be removed: {e:#}");
        }
    }
    Ok(())
}

/// A 16-byte value as hex.
mod hex16 {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Writes `value` as hex.
    pub(super) fn serialize<S: Serializer>(value: &[u8; 16], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(value))
    }

    /// Reads a value `serialize` wrote.
    /// Error cases:
    /// - the value is not the hex of 16 bytes
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 16], D::Error> {
        let mut value = [0; 16];
        hex::decode_to_slice(String::deserialize(deserializer)?, &mut value).map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}
