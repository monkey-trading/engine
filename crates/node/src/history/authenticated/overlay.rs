//! Bounded private-stage overlay: proved old values and locally known writes only.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::authenticated::{Lease, Root, Session, Store};
use crate::history::authenticated::model::Stored;

pub(super) struct Overlay {
    store: Store,
    root: Root,
    /// One read connection and transaction for every old value a private stage proves, when it has one.
    session: Option<Session>,
    reads: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    writes: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    /// Bytes of every value held so far, proved old values and staged ones, rewrites included.
    held: usize,
}

impl Overlay {
    /// Fixes the exact old root for one bounded proof-verified private update.
    pub fn new(store: &Store, root: &Root) -> Self {
        Self {
            store: store.clone(),
            root: root.clone(),
            session: None,
            reads: BTreeMap::new(),
            writes: BTreeMap::new(),
            held: 0,
        }
    }

    /// As `new` at `pin`'s root, proving every old value over one read session of it, which `finish`
    /// ends before the stage is written.
    pub fn session(store: &Store, pin: &Lease) -> Self {
        Self { session: Some(pin.session()), ..Self::new(store, pin.root()) }
    }

    /// Decodes a local known write or a point-proved old value/absence.
    /// Error cases:
    /// - storage, proof or decoding failure
    pub fn read<T: Stored>(&mut self, key: &[u8]) -> Result<Option<T>> {
        self.read_bytes(key)?.map(|bytes| T::decode(&bytes).context("decoding history value")).transpose()
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
    /// - old-value proof fails
    pub fn put<T: Stored>(&mut self, key: Vec<u8>, value: &T) -> Result<()> {
        self.put_bytes(key, value.encode())
    }

    /// Stages exact raw payload bytes after proving the old value/absence.
    /// Error cases:
    /// - old-value proof fails
    pub fn put_bytes(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        self.prove_old(&key)?;
        self.held = self.held.saturating_add(value.len());
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

    /// Counts the bytes of the values held, proved old values and staged ones, rewrites included, for
    /// staging's memory bound.
    pub fn held_bytes(&self) -> usize {
        self.held
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

    /// Emits one sorted unique exact update set and omits final no-ops, ending the read session.
    pub fn finish(self) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        self.writes.into_iter().filter(|(key, value)| self.reads.get(key) != Some(value)).collect()
    }

    /// Records only Store-proved old values or nonmembership.
    /// Error cases:
    /// - any storage/proof error
    fn prove_old(&mut self, key: &[u8]) -> Result<()> {
        if !self.reads.contains_key(key) {
            let value = match &mut self.session {
                Some(session) => session.read(key),
                None => self.store.read(&self.root, key),
            };
            let value = value.context("proving old history value")?;
            self.held = self.held.saturating_add(value.as_ref().map_or(0, Vec::len));
            self.reads.insert(key.to_vec(), value);
        }
        Ok(())
    }
}
