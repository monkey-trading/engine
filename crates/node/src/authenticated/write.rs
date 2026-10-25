//! Large logical updates share one durable SQL commit over bounded upstream proof batches.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use anyhow::{Result, ensure};
use rusqlite::{Connection, TransactionBehavior};

use crate::authenticated::blobs::{self, Compare};
use crate::authenticated::staging::Staging;
use crate::authenticated::{Root, Store, caller_key, db, geometry};

impl Store {
    /// Applies every exact caller change atomically, chunking physical values and JMT batches internally.
    /// Error cases:
    /// - duplicate/reserved keys, representational overflow, invalid proofs/storage or durability failure
    pub fn update(&self, root: &Root, changes: Vec<(Vec<u8>, Option<Vec<u8>>)>) -> Result<Root> {
        self.update_checked(root, changes, |_| Ok(()), || Ok(()))
    }

    /// Writes one arbitrary-length value from a finite caller source without collecting it in memory.
    /// Error cases:
    /// - short/excess source, reserved key, failed proof, version collision or durability failure
    pub fn write_stream(&self, root: &Root, key: &[u8], length: u64, mut source: impl Read) -> Result<Root> {
        let key = caller_key(&root.namespace, key)?;
        self.write_transaction(
            root,
            |tx| {
                let mut staging = Staging::new(tx, root)?;
                staging.value(key, Some((length, &mut source)))?;
                let written = staging.finish()?;
                ensure!(
                    blobs::read_to(&mut blobs::Points::new(tx, &written), key, &mut std::io::sink())? == Some(length),
                    "streamed value mismatch"
                );
                Ok(written)
            },
            |_| Ok(()),
            || Ok(()),
        )
    }

    /// Runs the same write path with private before/after-commit failure seams for regression tests.
    /// Error cases:
    /// - any exact change, proof, SQL, hook, commit or sync check fails
    pub(super) fn update_checked(
        &self,
        root: &Root,
        changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        before_commit: impl FnOnce(&Connection) -> Result<()>,
        after_commit: impl FnOnce() -> Result<()>,
    ) -> Result<Root> {
        let mut sorted = BTreeMap::new();
        for (key, value) in changes {
            let hash = caller_key(&root.namespace, &key)?;
            ensure!(sorted.insert(hash, value).is_none(), "duplicate caller key or key hash");
        }
        self.write_transaction(
            root,
            |tx| {
                let mut staging = Staging::new(tx, root)?;
                for (key, value) in &sorted {
                    let mut source = Cursor::new(value.as_deref().unwrap_or_default());
                    let input = value.as_ref().map(|bytes| (bytes.len() as u64, &mut source));
                    staging.value(*key, input)?;
                }
                let written = staging.finish()?;
                let mut points = blobs::Points::new(tx, &written);
                for (key, expected) in &sorted {
                    if let Some(expected) = expected {
                        let mut compare = Compare { expected, offset: 0 };
                        ensure!(
                            blobs::read_to(&mut points, *key, &mut compare)? == Some(expected.len() as u64)
                                && compare.offset == expected.len(),
                            "persisted exact caller value mismatch"
                        );
                    } else {
                        ensure!(
                            blobs::descriptor(&mut points, *key)?.is_none(),
                            "deleted caller value remains present"
                        );
                    }
                }
                Ok(written)
            },
            before_commit,
            after_commit,
        )
    }

    /// Serializes mutation and acknowledges the final logical root only after commit and directory sync.
    /// Error cases:
    /// - writes unauthorized, stale/retired root, poisoned store, malformed geometry or failed work/durability
    pub(super) fn write_transaction(
        &self,
        root: &Root,
        work: impl FnOnce(&Connection) -> Result<Root>,
        before_commit: impl FnOnce(&Connection) -> Result<()>,
        after_commit: impl FnOnce() -> Result<()>,
    ) -> Result<Root> {
        root.validate()?;
        let _writer = self.inner.writer.lock().map_err(|_| anyhow::anyhow!("writer poisoned"))?;
        self.writable()?;
        let _lease = self.lease(root)?;
        let old_floor = self.check_head(root)?;
        geometry::check(&self.inner.path)?;
        let mut conn = db::connect(&self.inner.path, true)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        db::validate(&tx)?;
        let written = match work(&tx).and_then(|written| {
            before_commit(&tx)?;
            Ok(written)
        }) {
            Ok(written) => written,
            Err(error) => {
                if let Err(rollback) = tx.rollback() {
                    self.inner.runtime.lock().map_err(|_| anyhow::anyhow!("lease registry poisoned"))?.poisoned = true;
                    return Err(error.context(format!("authenticated transaction rollback failed: {rollback}")));
                }
                if let Some(namespace) = self
                    .inner
                    .runtime
                    .lock()
                    .map_err(|_| anyhow::anyhow!("lease registry poisoned"))?
                    .namespaces
                    .get_mut(&root.namespace)
                {
                    namespace.floor = old_floor;
                }
                return Err(error);
            }
        };
        if let Err(error) =
            tx.commit().map_err(anyhow::Error::from).and_then(|()| after_commit()).and_then(|()| self.sync())
        {
            self.inner.runtime.lock().map_err(|_| anyhow::anyhow!("lease registry poisoned"))?.poisoned = true;
            return Err(error);
        }
        self.publish(&written)?;
        Ok(written)
    }

    /// Deletes every version after `root` in its namespace, so writes resume from that root. The
    /// caller passes only a root it authenticated, after recovery preflight. Later versions cannot
    /// have collected that root's data: the writer kept the root pinned while it wrote them. Every
    /// commit writes nodes at its version, so a store with none after `root` is left untouched;
    /// otherwise both deletes are range deletes on a version index.
    /// Error cases:
    /// - writes are unauthorized, another in-process head exists, or storage or durability fails
    pub fn discard_after(&self, root: &Root) -> Result<()> {
        root.validate()?;
        let _writer = self.inner.writer.lock().map_err(|_| anyhow::anyhow!("writer poisoned"))?;
        self.writable()?;
        self.check_head(root)?;
        geometry::check(&self.inner.path)?;
        let mut conn = db::connect(&self.inner.path, true)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        db::validate(&tx)?;
        let after = root.version.map_or(Ok(-1), i64::try_from)?;
        let later: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM nodes WHERE namespace = ?1 AND version > ?2)",
            rusqlite::params![root.namespace.as_slice(), after],
            |row| row.get(0),
        )?;
        if !later {
            tx.rollback()?;
            return self.publish(root);
        }
        tx.execute(
            "DELETE FROM nodes WHERE namespace = ?1 AND version > ?2",
            rusqlite::params![root.namespace.as_slice(), after],
        )?;
        tx.execute(
            "DELETE FROM value_rows WHERE namespace = ?1 AND version > ?2",
            rusqlite::params![root.namespace.as_slice(), after],
        )?;
        if let Err(error) = tx.commit().map_err(anyhow::Error::from).and_then(|()| self.sync()) {
            self.inner.runtime.lock().map_err(|_| anyhow::anyhow!("lease registry poisoned"))?.poisoned = true;
            return Err(error);
        }
        self.publish(root)
    }

    /// Whether a commit's outcome was uncertain, so this store refuses writes until it is reopened.
    /// A failed write that rolled back cleanly leaves it writable from the same root.
    pub fn requires_reopen(&self) -> bool {
        self.inner.runtime.lock().map_or(true, |runtime| runtime.poisoned)
    }

    /// Checks solely the trusted in-process writer head; disk never nominates a latest root.
    /// Error cases:
    /// - poisoned runtime or an attempted fork from another in-process head
    pub(super) fn check_head(&self, root: &Root) -> Result<u64> {
        let runtime = self.inner.runtime.lock().map_err(|_| anyhow::anyhow!("lease registry poisoned"))?;
        ensure!(!runtime.poisoned, "store requires reopen after uncertain commit");
        let head = runtime.namespaces.get(&root.namespace).and_then(|ns| ns.head.as_ref());
        ensure!(head.is_none_or(|head| head == root), "write root is not current");
        Ok(runtime.namespaces.get(&root.namespace).map_or(0, |namespace| namespace.floor))
    }

    /// Advances the private live root only after its durable acknowledgement boundary.
    /// Error cases:
    /// - poisoned runtime lock
    pub(super) fn publish(&self, root: &Root) -> Result<()> {
        self.inner
            .runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("lease registry poisoned"))?
            .namespaces
            .entry(root.namespace)
            .or_default()
            .head = Some(root.clone());
        Ok(())
    }
}
