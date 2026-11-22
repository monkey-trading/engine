//! Bounded physical proof batches share one SQL transaction and remain unpublished until complete.

use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Result, ensure};
use jmt::{KeyHash, SimpleHasher};
use rusqlite::Connection;

use crate::authenticated::blobs::{self, Blob, Points, Stored};
use crate::authenticated::reader::Checked;
use crate::authenticated::receipts::{self, Queue};
use crate::authenticated::{CHUNK_BYTES, Hasher, MAX_UPDATES, Root, batch};

pub(super) struct Staging<'a> {
    conn: &'a Connection,
    /// The nodes the update has checked, which every batch's reads share.
    checked: &'a Checked,
    pub root: Root,
    /// Descriptor reads under the current staged root, renewed whenever a batch advances it.
    points: Points<'a>,
    queue: Queue,
    pending: BTreeMap<KeyHash, Option<Vec<u8>>>,
    wrote: bool,
    /// Whether the update writes its nodes compact and each value of at most one chunk whole in its leaf.
    compact: bool,
}

impl<'a> Staging<'a> {
    /// Proves the original maintenance queue before accepting logical changes, sharing the nodes
    /// `checked` across the update's reads, and writing `compact`: its nodes in borsh form and each value of
    /// at most one chunk whole in its leaf, or its nodes as JSON and every value in chunks.
    /// Error cases:
    /// - a queue/root proof or cursor check fails
    pub(super) fn new(conn: &'a Connection, root: &Root, checked: &'a Checked, compact: bool) -> Result<Self> {
        Ok(Self {
            conn,
            checked,
            root: root.clone(),
            points: Points::new(conn, root, checked),
            queue: receipts::queue(conn, root)?,
            pending: BTreeMap::new(),
            wrote: false,
            compact,
        })
    }

    /// Stages an exact length-delimited logical value or deletion: whole in its own leaf when the update
    /// writes compact and it fits one chunk, otherwise as a descriptor and finite physical chunks.
    /// Error cases:
    /// - invalid old descriptor, incomplete/excess source bytes, failed proofs or persistence failure
    pub(super) fn value(&mut self, key: KeyHash, input: Option<(u64, &mut impl Read)>) -> Result<()> {
        let old = blobs::descriptor(&mut self.points, key)?;
        let old_chunks = old.as_ref().map_or(0, Stored::chunks);
        let (leaf, new_chunks) = match input {
            Some((length, source)) if self.compact && length <= CHUNK_BYTES as u64 => {
                let bytes = blobs::input_chunk(source, length)?;
                drained(source)?;
                (Some(blobs::inline(&bytes)), 0)
            }
            Some((length, source)) => {
                let mut remaining = length;
                let mut index = 0u64;
                let mut hasher = <Hasher as SimpleHasher>::new();
                while remaining != 0 {
                    let bytes = blobs::input_chunk(source, remaining)?;
                    hasher.update(&bytes);
                    remaining -= bytes.len() as u64;
                    self.push(blobs::chunk_key(&self.root.namespace, key, index)?, Some(bytes))?;
                    index = index.checked_add(1).ok_or_else(|| anyhow::anyhow!("chunk ordinal overflow"))?;
                }
                drained(source)?;
                let blob = Blob { length, digest: hasher.finalize() };
                (Some(serde_json::to_vec(&blob)?), blob.chunks())
            }
            None => (None, 0),
        };
        for index in new_chunks..old_chunks {
            self.push(blobs::chunk_key(&self.root.namespace, key, index)?, None)?;
        }
        self.push(key, leaf)?;
        Ok(())
    }

    /// Adds a single physical change; a full finite batch is verified and persisted before continuing.
    /// Error cases:
    /// - internal key collision or failed proof/persistence
    fn push(&mut self, key: KeyHash, value: Option<Vec<u8>>) -> Result<()> {
        ensure!(!self.pending.contains_key(&key), "duplicate staged physical key");
        self.pending.insert(key, value);
        if self.pending.len() == MAX_UPDATES {
            self.flush(false)?;
        }
        Ok(())
    }

    /// Completes every staged change; even an empty logical update materializes one version.
    /// Error cases:
    /// - failed final proof batch or immutable-version collision
    pub(super) fn finish(mut self) -> Result<Root> {
        self.flush(!self.wrote)?;
        Ok(self.root)
    }

    /// Uses two-pass authenticated receipt construction without committing the outer SQL transaction.
    /// Error cases:
    /// - failed transition/provenance verification or immutable persistence collision
    fn flush(&mut self, force: bool) -> Result<()> {
        if self.pending.is_empty() && !force {
            return Ok(());
        }
        let changes = std::mem::take(&mut self.pending).into_iter().collect();
        let prepared = batch::prepare(self.conn, &self.root, changes, self.queue.clone(), false, self.checked)?;
        batch::persist(self.conn, &prepared, self.checked, self.compact)?;
        self.root = prepared.root;
        self.points = Points::new(self.conn, &self.root, self.checked);
        self.queue.hi = self.root.version.ok_or_else(|| anyhow::anyhow!("missing staged version"))? + 1;
        self.wrote = true;
        Ok(())
    }
}

/// Checks that `source` holds nothing past the value's declared length.
/// Error cases:
/// - the source holds more, or cannot be read
fn drained(source: &mut impl Read) -> Result<()> {
    let mut extra = [0u8; 1];
    ensure!(source.read(&mut extra)? == 0, "source exceeds declared logical length");
    Ok(())
}
