//! Frozen copies of the marks cache at a daily snapshot's checkpoint. The marks task writes one once the cache
//! holds the marks of every command the checkpoint covers (`Store::freeze`): a copy of the file beside it
//! (`VACUUM INTO`, synced in steps as it is written), named for the checkpoint's offset, of which those of the newest
//! `KEPT` listed snapshots and of the one being written stay (`Store::freeze_keeping`), and the cache's committed
//! chain anchors as they stand then with the root over the copy's rows (`Frozen`), which the freeze finds by reading
//! the copy back under the key that wrote it (`walk`, `root`). The snapshot carries both under its signature. A start
//! from the snapshot takes the copy in place of the cache (`Store::import_frozen`) only once every pair's chain in it
//! is the one those anchors name and its rows, read in order, give that root. The check needs no key, so a start
//! under keys sealed afresh takes the copy as a start under the keys that wrote it does. A copy the host rolled back,
//! or altered in any row, is refused whole. A snapshot written before it carried the root is taken only under the key
//! that wrote its copy, every row's tag checked.

mod root;
mod walk;

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, ensure};
use engine_core::eip712::B256;
use engine_core::types::{MarketId, Millis};
use rand::RngCore;
use rand::rngs::OsRng;
use rusqlite::{TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::history::marks::store::auth::{Epoch, Row, run_mac};
use crate::history::marks::store::chain::anchor_updates;
use crate::history::marks::store::frozen::walk::{Passed, open_copy, walk};
use crate::history::marks::store::setup::read_epoch;
use crate::history::marks::store::{ChainHead, Store, store, tag};
use crate::log::Offset;
use crate::paced::STEP;

/// How many frozen copies stay beside the cache: a start takes the newest snapshot it can, and the copies of the
/// days before it serve a start that passes the newest over.
pub(crate) const KEPT: usize = 3;
/// What a frozen copy's name starts with; its checkpoint's offset follows, as a daily snapshot's name gives it.
const PREFIX: &str = "marks-frozen-";

/// How often the copy SQLite writes is checked for a step of bytes written since its last sync (`paced`).
const PACE_POLL: Duration = Duration::from_millis(10);

/// The cache's committed chain anchors as a frozen copy holds the cache: the copy's run nonce and each pair's, and the
/// root over the copy's rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Frozen {
    #[serde(with = "hex16")]
    pub epoch: Epoch,
    pub pairs: Vec<FrozenPair>,
    /// The root over the copy's rows (`root`), which a start under any key checks them against; none in a snapshot
    /// written before it was carried, whose copy a start takes only under the key that wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<B256>,
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

impl Store {
    /// `freeze_keeping` with no daily snapshots' directory, which keeps the newest `KEPT` copies.
    /// Error cases:
    /// - as `freeze_keeping`
    #[cfg(test)]
    pub(crate) fn freeze(&self, offset: Offset) -> anyhow::Result<Frozen> {
        self.freeze_keeping(offset, None)
    }

    /// Writes a frozen copy of the cache beside it for the checkpoint at `offset`, synced as it is written (`paced`)
    /// and at its end, and gives the committed anchors it holds with the root over its rows, read from the copy once
    /// it is synced and before it takes its name, each row's tag checked under this cache's run key and each pair's
    /// chain against the anchors (`walk`): the root covers only rows this node wrote, as the anchors name them. The
    /// cache's writes all run on this store, so nothing commits between the copy and the anchors read after it. It
    /// removes the other copies but, with `snapshots`, the directory of the daily snapshots, those of the newest
    /// `KEPT` snapshots it lists and this one, whose snapshot is not listed yet, or without it the newest `KEPT`: a
    /// copy whose snapshot was never published, as when its writing failed, then takes no place a published one needs.
    /// Error cases:
    /// - the copy cannot be written, synced or renamed into place, or its directory listed
    /// - the copy does not read back as the cache's rows under its anchors
    pub(crate) fn freeze_keeping(&self, offset: Offset, snapshots: Option<&Path>) -> anyhow::Result<Frozen> {
        let dir = self.path.parent().context("the marks cache has no directory")?;
        let path = frozen_path(dir, offset);
        let staged = path.with_extension("tmp");
        store::discard(&staged)?;
        let target = staged.to_str().context("the frozen copy's path is not UTF-8")?;
        paced(&staged, || self.conn.execute("VACUUM INTO ?1", [target])).context("copying the marks cache")?;
        File::open(&staged).and_then(|file| file.sync_all()).context("syncing the frozen marks cache")?;
        let mut frozen = self.anchored(read_epoch(&self.conn)?);
        let copy = open_copy(&staged, &self.schema, &frozen).context("reading the frozen marks cache back")?;
        let (_, root) = walk(&copy, &frozen, Some(&self.mac), |passed| Ok(passed.row.tag))
            .context("reading the frozen marks cache back")?;
        drop(copy);
        frozen.root = Some(root);
        fs::rename(&staged, &path).with_context(|| format!("publishing {}", path.display()))?;
        File::open(dir).and_then(|opened| opened.sync_all()).with_context(|| format!("syncing {}", dir.display()))?;
        prune(dir, snapshots.and_then(|snapshots| listed_copies(dir, snapshots, offset)))?;
        Ok(frozen)
    }

    /// The committed anchors of every pair as they stand, under the run nonce `epoch`, with no root yet.
    fn anchored(&self, epoch: Epoch) -> Frozen {
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
        Frozen { epoch, pairs, root: None }
    }

    /// Imports the frozen copy at `copy` into this cache, which holds nothing yet, once its every pair's chain is
    /// the one `frozen` names and its rows are the ones `frozen`'s root covers: each row names the row below it and
    /// binds the tag that row carries, unless that row is pinned, every pinned row carries its pin, each pair's floor
    /// and head are its anchor's, with no pair more or fewer, and the root over the rows read, in order, is
    /// `frozen`'s. That holds under any key, so a start under keys sealed afresh takes the copy. For a snapshot that
    /// carries no root, each row's tag must instead authenticate under the copy's run nonce, `frozen`'s, and this
    /// cache's marks key, as the key that wrote it. Each row is re-tagged as it is taken, as a prior cache's rows are
    /// (`import_prior`), under a run nonce drawn for the import, which the cache takes only once every row is proven:
    /// rows proven only at the end, against the root, are written before then, and SQLite may spill them to the
    /// host's disk, so no tag under a nonce the cache keeps is ever given to a row before it is proven. With the
    /// backfill markers, in one transaction, which a refusal rolls back, so a row that fails leaves nothing imported
    /// and the cache's nonce as it was; the copy is opened read-only and never modified.
    /// Error cases:
    /// - the copy cannot be read, holds another schema or another run nonce, or a row or pair fails the check
    /// - the rows read are not the ones `frozen`'s root covers
    /// - the import cannot be written
    pub(crate) fn import_frozen(&mut self, copy: &Path, frozen: &Frozen) -> anyhow::Result<usize> {
        let from = open_copy(copy, &self.schema, frozen)?;
        let tags = frozen.root.is_none().then(|| run_mac(&self.mac_key, &frozen.epoch));
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
        let mut nonce = Epoch::default();
        OsRng.fill_bytes(&mut nonce);
        let (next, mut imported) = (run_mac(&self.mac_key, &nonce), 0);
        let (pairs, root) = walk(&from, frozen, tags.as_ref(), |Passed { market, resolution, row, prev_tag }| {
            let retagged = tag(&next, market, resolution, row.bucket, &Row { prev_tag, ..row.fields() });
            insert
                .execute(params![
                    i64::from(market),
                    resolution as i64,
                    row.bucket,
                    row.open,
                    row.high,
                    row.low,
                    row.close,
                    row.provenance,
                    row.prev,
                    retagged,
                    prev_tag
                ])
                .with_context(|| format!("importing the frozen {market} {resolution:?} bucket at {}", row.bucket))?;
            imported += 1;
            Ok(retagged)
        })?;
        if let Some(signed) = frozen.root {
            ensure!(root == signed, "its rows are not the ones the daily snapshot's root covers");
        }
        drop(insert);
        tx.execute("UPDATE epoch SET nonce = ?1 WHERE id = 0", [nonce]).context("drawing the run nonce")?;
        copy_markers(&from, &tx)?;
        let updates = anchor_updates(&tx, &pairs, &Default::default(), false)?;
        let mut committed = self.anchors.lock().expect("the marks chain anchors are never poisoned");
        tx.commit().context("committing the frozen marks import")?;
        committed.apply(updates);
        drop(committed);
        self.mac = next;
        Ok(imported)
    }
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

/// Runs `write`, which writes the file at `path`, while syncing the file's data each time it has grown a step
/// (`paced::STEP`) past its last sync, as found every `PACE_POLL`: SQLite writes the copy itself, so the syncs a
/// `Paced` writer makes between its steps are made beside it, and what is dirty stays within a step and what one
/// poll's writing adds, never the whole copy behind the log's syncs. A sync that fails is left to the copy's own
/// at its end.
fn paced<T>(path: &Path, write: impl FnOnce() -> T) -> T {
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let (mut file, mut synced) = (None, 0u64);
            while !done.load(Ordering::Acquire) {
                std::thread::sleep(PACE_POLL);
                if file.is_none() {
                    file = File::open(path).ok();
                }
                let Some(opened) = &file else { continue };
                let Ok(len) = opened.metadata().map(|metadata| metadata.len()) else { continue };
                if len >= synced + STEP as u64 && opened.sync_data().is_ok() {
                    synced = len;
                }
            }
        });
        let written = write();
        done.store(true, Ordering::Release);
        written
    })
}

/// The frozen copies in `dir` a start from the daily snapshots in `snapshots` may take: those of the newest `KEPT` it
/// lists, and that at `offset`, whose snapshot is not listed yet; none when the listing cannot be read, which keeps
/// the newest `KEPT` copies.
fn listed_copies(dir: &Path, snapshots: &Path, offset: Offset) -> Option<BTreeSet<PathBuf>> {
    let listed = crate::daily_snapshots::read_manifest(snapshots)
        .inspect_err(|e| warn!("the daily snapshots cannot be listed for their frozen marks copies: {e:#}"))
        .ok()?;
    let newest = listed.iter().rev().take(KEPT).map(|listing| Offset { seq: listing.seq, index: listing.index });
    Some(newest.chain(std::iter::once(offset)).map(|offset| frozen_path(dir, offset)).collect())
}

/// Removes the frozen copies in `dir` but those in `keep`, or without it past the newest `KEPT`, and any a crash left
/// staged.
/// Error cases:
/// - the directory cannot be listed
fn prune(dir: &Path, keep: Option<BTreeSet<PathBuf>>) -> anyhow::Result<()> {
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
    let held_copies = copies.iter().filter(|(staged, _)| !staged).map(|(_, path)| path);
    let stale: Vec<&PathBuf> = match &keep {
        Some(keep) => held_copies.filter(|path| !keep.contains(*path)).collect(),
        None => held_copies.skip(KEPT).collect(),
    };
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
