//! Bounded private-stage overlay: proved old values and locally known writes only.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::authenticated::{Root, Store};

pub(super) struct Overlay {
    store: Store,
    root: Root,
    reads: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl Overlay {
    /// Fixes the exact old root for one bounded proof-verified private update.
    pub fn new(store: &Store, root: &Root) -> Self {
        Self { store: store.clone(), root: root.clone(), reads: BTreeMap::new(), writes: BTreeMap::new() }
    }

    /// Decodes a local known write or a point-proved old value/absence.
    /// Error cases:
    /// - storage, proof or decoding failure
    pub fn read<T: DeserializeOwned>(&mut self, key: &[u8]) -> Result<Option<T>> {
        self.read_bytes(key)?.map(|bytes| serde_json::from_slice(&bytes).context("decoding history value")).transpose()
    }

    /// Reads exact chunk bytes, always against the pinned old root or a known local write.
    /// Error cases:
    /// - storage or membership/nonmembership proof fails
    pub fn read_bytes(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(value) = self.writes.get(key) {
            return Ok(value.clone());
        }
        self.prove_old(key)?;
        Ok(self.reads.get(key).expect("proved result is recorded").clone())
    }

    /// Stages a bounded structured header/state after proving the old value/absence.
    /// Error cases:
    /// - encoding or old-value proof fails
    pub fn put<T: Serialize>(&mut self, key: Vec<u8>, value: &T) -> Result<()> {
        self.put_bytes(key, serde_json::to_vec(value).context("encoding history mutation")?)
    }

    /// Stages exact raw payload bytes after proving the old value/absence.
    /// Error cases:
    /// - old-value proof fails
    pub fn put_bytes(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        self.prove_old(&key)?;
        self.writes.insert(key, Some(value));
        Ok(())
    }

    /// Stages deletion only after a verified old read.
    /// Error cases:
    /// - membership/nonmembership proof fails
    pub fn delete(&mut self, key: Vec<u8>) -> Result<()> {
        self.prove_old(&key)?;
        self.writes.insert(key, None);
        Ok(())
    }

    /// Counts locally distinct writes for staging, never as a retention/event policy limit.
    pub fn len(&self) -> usize {
        self.writes.len()
    }

    /// Counts buffered key/value bytes for the source-level bounded-staging regression.
    #[cfg(test)]
    pub fn buffered_bytes(&self) -> usize {
        self.reads
            .iter()
            .chain(&self.writes)
            .map(|(key, value)| key.len().saturating_add(value.as_ref().map_or(0, Vec::len)))
            .fold(0usize, usize::saturating_add)
    }

    /// Emits one sorted unique exact update set and omits final no-ops.
    pub fn finish(self) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        self.writes.into_iter().filter(|(key, value)| self.reads.get(key) != Some(value)).collect()
    }

    /// Records only Store-proved old values or nonmembership.
    /// Error cases:
    /// - any storage/proof error
    fn prove_old(&mut self, key: &[u8]) -> Result<()> {
        if !self.reads.contains_key(key) {
            let value = self.store.read(&self.root, key).context("proving old history value")?;
            self.reads.insert(key.to_vec(), value);
        }
        Ok(())
    }
}
