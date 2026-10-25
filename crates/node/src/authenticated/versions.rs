//! Cancellation-safe pins with no retention/lease-count cap and logarithmic floor lookup.

use std::sync::{Arc, Weak};

use anyhow::{Result, ensure};

use crate::authenticated::{Inner, Root, Store, blobs, caller_key, db};

/// A blocking reader/snapshot owns a clone until it actually finishes, independently of async waiters.
#[derive(Clone)]
pub struct Lease {
    store: Store,
    pin: Arc<Pin>,
}

pub(super) struct Pin {
    root: Root,
    owner: Weak<Inner>,
}

impl Drop for Pin {
    /// Removes only this allocation's weak entry, preserving a concurrently admitted replacement.
    fn drop(&mut self) {
        let Some(owner) = self.owner.upgrade() else { return };
        // A poisoned registry already fails closed; inability to unpin leaks rather than authorizes GC.
        let Ok(mut runtime) = owner.runtime.lock() else { return };
        let Some(namespace) = runtime.namespaces.get_mut(&self.root.namespace) else { return };
        if namespace.pins.get(&self.root).is_some_and(|weak| weak.as_ptr() == std::ptr::from_ref(self)) {
            namespace.pins.remove(&self.root);
        }
        if let Some(version) = self.root.version
            && let Some(count) = namespace.versions.get_mut(&version)
        {
            *count -= 1;
            if *count == 0 {
                namespace.versions.remove(&version);
            }
        }
        if namespace.head.is_none() && namespace.floor == 0 && namespace.pins.is_empty() {
            runtime.namespaces.remove(&self.root.namespace);
        }
    }
}

impl Store {
    /// Refuses a stale write base by an indexed existence probe; disk cannot nominate a new root.
    /// Error cases:
    /// - invalid root/schema, storage IO, or any physical node version after this certificate
    pub fn ensure_current(&self, root: &Root) -> Result<()> {
        root.validate()?;
        let mut connection = self.read_connection()?;
        let transaction = connection.transaction()?;
        db::validate(&transaction)?;
        let later: bool = match root.version {
            Some(version) => transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM nodes WHERE namespace = ?1 AND version > ?2 LIMIT 1)",
                rusqlite::params![root.namespace.as_slice(), i64::try_from(version)?],
                |row| row.get(0),
            )?,
            None => transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM nodes WHERE namespace = ?1 LIMIT 1)",
                [root.namespace.as_slice()],
                |row| row.get(0),
            )?,
        };
        ensure!(!later, "unacknowledged versions follow the certified cache head");
        transaction.commit()?;
        Ok(())
    }

    /// Pins an authenticated root without disk IO or an arbitrary cache/retention cap.
    /// Error cases:
    /// - invalid/retired root, poisoned registry or native pin-counter overflow
    pub fn lease(&self, root: &Root) -> Result<Lease> {
        root.validate()?;
        let mut runtime = self.inner.runtime.lock().map_err(|_| anyhow::anyhow!("lease registry poisoned"))?;
        let namespace = runtime.namespaces.entry(root.namespace).or_default();
        ensure!(root.version.is_none_or(|v| v >= namespace.floor), "version retired");
        let existing = namespace.pins.get(root).and_then(Weak::upgrade);
        let pin = if let Some(pin) = existing {
            pin
        } else {
            if let Some(version) = root.version {
                let count = namespace.versions.entry(version).or_default();
                *count = count.checked_add(1).ok_or_else(|| anyhow::anyhow!("native pin count overflow"))?;
            }
            let pin = Arc::new(Pin { root: root.clone(), owner: Arc::downgrade(&self.inner) });
            namespace.pins.insert(root.clone(), Arc::downgrade(&pin));
            pin
        };
        drop(runtime);
        Ok(Lease { store: self.clone(), pin })
    }

    /// Pins every retained certificate root before collection; caller retains all returned handles.
    /// Error cases:
    /// - any root cannot be leased; temporary pins are released if the complete operation fails
    pub fn pin_versions(&self, roots: &[Root]) -> Result<Vec<Lease>> {
        roots.iter().map(|root| self.lease(root)).collect()
    }

    /// Reads the exact minimum of all version pins without scanning or advancing admission.
    /// Error cases:
    /// - poisoned registry or a pre-version collection request
    pub(super) fn floor_snapshot(&self, root: &Root) -> Result<u64> {
        let version = root.version.ok_or_else(|| anyhow::anyhow!("unversioned collection"))?;
        let runtime = self.inner.runtime.lock().map_err(|_| anyhow::anyhow!("lease registry poisoned"))?;
        Ok(runtime
            .namespaces
            .get(&root.namespace)
            .and_then(|ns| ns.versions.first_key_value())
            .map_or(version, |(v, _)| (*v).min(version)))
    }

    /// Reserves lease admission only if every selected retirement is still protected from new old pins.
    /// Error cases:
    /// - poisoned registry or a pre-version collection request
    pub(super) fn reserve_collection(&self, root: &Root, since: u64) -> Result<Option<u64>> {
        let version = root.version.ok_or_else(|| anyhow::anyhow!("unversioned collection"))?;
        let mut runtime = self.inner.runtime.lock().map_err(|_| anyhow::anyhow!("lease registry poisoned"))?;
        let namespace = runtime.namespaces.entry(root.namespace).or_default();
        let floor = namespace.versions.first_key_value().map_or(version, |(v, _)| (*v).min(version));
        if since > floor {
            return Ok(None);
        }
        namespace.floor = namespace.floor.max(floor);
        Ok(Some(floor))
    }
}

impl Lease {
    /// Returns this handle's exact immutable root.
    pub fn root(&self) -> &Root {
        &self.pin.root
    }

    /// Returns a complete logical value only after all chunks and its descriptor are verified.
    /// Error cases:
    /// - proof/format/allocation failure, unavailable storage or reserved caller key
    pub fn read(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let root = self.root();
        let key = caller_key(&root.namespace, key)?;
        let mut conn = self.store.read_connection()?;
        let tx = conn.transaction()?;
        db::validate(&tx)?;
        let value = blobs::read(&mut blobs::Points::new(&tx, root), key)?;
        tx.commit()?;
        Ok(value)
    }

    /// Streams proved chunks without allocating the logical length; output is provisional until success.
    /// Error cases:
    /// - failed proof/length/digest, unavailable storage, reserved key or sink error
    pub fn read_to(&self, key: &[u8], mut sink: impl std::io::Write) -> Result<Option<u64>> {
        let root = self.root();
        let key = caller_key(&root.namespace, key)?;
        let mut conn = self.store.read_connection()?;
        let tx = conn.transaction()?;
        db::validate(&tx)?;
        let length = blobs::read_to(&mut blobs::Points::new(&tx, root), key, &mut sink)?;
        tx.commit()?;
        Ok(length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_dir;
    use std::sync::{Arc, Barrier};

    #[test]
    fn current_probe_refuses_unacknowledged_versions_without_selecting_a_root() {
        let dir = scratch_dir("authenticated-current-probe");
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::create(dir.join("cache.sqlite")).unwrap();
        let empty = Root::empty([31; 32]);
        store.ensure_current(&empty).unwrap();
        let first = store.update(&empty, vec![(b"key".to_vec(), Some(b"old".to_vec()))]).unwrap();
        let pin = store.lease(&first).unwrap();
        store.ensure_current(&first).unwrap();
        assert!(store.ensure_current(&empty).is_err());
        let last = store.update(&first, vec![(b"key".to_vec(), Some(b"new".to_vec()))]).unwrap();
        assert!(store.ensure_current(&first).is_err());
        assert_eq!(pin.read(b"key").unwrap(), Some(b"old".to_vec()));
        store.ensure_current(&last).unwrap();
        let connection = db::connect(&store.inner.path, false).unwrap();
        let plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT EXISTS(SELECT 1 FROM nodes WHERE namespace = ?1 AND version > ?2 LIMIT 1)",
                rusqlite::params![first.namespace.as_slice(), 0_i64],
                |row| row.get(3),
            )
            .unwrap();
        // The first plan row may describe the scalar subquery. Inspect every plan detail.
        let mut statement = connection
            .prepare("EXPLAIN QUERY PLAN SELECT 1 FROM nodes WHERE namespace = ?1 AND version > ?2 LIMIT 1")
            .unwrap();
        let details = statement
            .query_map(rusqlite::params![first.namespace.as_slice(), 0_i64], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            details.iter().any(|detail| detail.contains("SEARCH nodes USING PRIMARY KEY")),
            "{plan:?}: {details:?}"
        );
        assert!(!details.iter().any(|detail| detail.contains("SCAN nodes")));
    }

    #[test]
    fn file_lifetime_waits_for_actual_blocking_reader_after_waiter_drops() {
        let dir = scratch_dir("authenticated-file-lifetime");
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::create(dir.join("cache.sqlite")).unwrap();
        let root = store.update(&Root::empty([32; 32]), vec![(b"key".to_vec(), Some(b"value".to_vec()))]).unwrap();
        let lifetime = store.lifetime();
        let lease = store.lease(&root).unwrap();
        let admitted = Arc::new(Barrier::new(2));
        let finish = Arc::new(Barrier::new(2));
        let (reader_admitted, reader_finish) = (admitted.clone(), finish.clone());
        let reader = std::thread::spawn(move || {
            reader_admitted.wait();
            reader_finish.wait();
            assert_eq!(lease.read(b"key").unwrap(), Some(b"value".to_vec()));
        });
        admitted.wait();
        drop(store);
        assert!(lifetime.alive(), "dropping the async owner must not retire the actual reader's file");
        finish.wait();
        reader.join().unwrap();
        assert!(!lifetime.alive(), "the retired file becomes eligible only after actual read completion");
    }
}
