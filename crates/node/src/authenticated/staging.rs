//! Bounded physical proof batches share one SQL transaction and remain unpublished until complete.

use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Result, ensure};
use jmt::{KeyHash, SimpleHasher};
use rusqlite::Connection;

use crate::authenticated::blobs::{self, Blob, Points};
use crate::authenticated::receipts::{self, Queue};
use crate::authenticated::{Hasher, MAX_UPDATES, Root, batch};

pub(super) struct Staging<'a> {
    conn: &'a Connection,
    pub root: Root,
    /// Descriptor reads under the current staged root, renewed whenever a batch advances it.
    points: Points<'a>,
    queue: Queue,
    pending: BTreeMap<KeyHash, Option<Vec<u8>>>,
    wrote: bool,
}

impl<'a> Staging<'a> {
    /// Proves the original maintenance queue before accepting logical changes.
    /// Error cases:
    /// - a queue/root proof or cursor check fails
    pub(super) fn new(conn: &'a Connection, root: &Root) -> Result<Self> {
        Ok(Self {
            conn,
            root: root.clone(),
            points: Points::new(conn, root),
            queue: receipts::queue(conn, root)?,
            pending: BTreeMap::new(),
            wrote: false,
        })
    }

    /// Stages an exact length-delimited logical value or deletion using finite physical chunks.
    /// Error cases:
    /// - invalid old descriptor, incomplete/excess source bytes, failed proofs or persistence failure
    pub(super) fn value(&mut self, key: KeyHash, input: Option<(u64, &mut impl Read)>) -> Result<()> {
        let old = blobs::descriptor(&mut self.points, key)?;
        let old_chunks = old.as_ref().map_or(0, Blob::chunks);
        let descriptor = if let Some((length, source)) = input {
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
            let mut extra = [0u8; 1];
            ensure!(source.read(&mut extra)? == 0, "source exceeds declared logical length");
            Some(Blob { length, digest: hasher.finalize() })
        } else {
            None
        };
        let new_chunks = descriptor.as_ref().map_or(0, Blob::chunks);
        for index in new_chunks..old_chunks {
            self.push(blobs::chunk_key(&self.root.namespace, key, index)?, None)?;
        }
        self.push(key, descriptor.map(|blob| serde_json::to_vec(&blob)).transpose()?)?;
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
        let prepared = batch::prepare(self.conn, &self.root, changes, self.queue.clone(), false)?;
        batch::persist(self.conn, &prepared)?;
        self.root = prepared.root;
        self.points = Points::new(self.conn, &self.root);
        self.queue.hi = self.root.version.ok_or_else(|| anyhow::anyhow!("missing staged version"))? + 1;
        self.wrote = true;
        Ok(())
    }
}
