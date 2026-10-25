//! Store reads, updates, and proofs against real temporary SQLite files.

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::Connection;

use crate::authenticated::{Root, Store};

pub(super) struct TestDir {
    pub path: PathBuf,
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn exact_values_nonmembership_deletion_and_old_versions() -> Result<()> {
    let (_dir, store) = fresh()?;
    let empty = Root::empty([1; 32]);
    assert_eq!(store.read(&empty, b"a")?, None);
    let first = store.update(&empty, vec![put(b"a", b"one"), put(b"b", b"two")])?;
    assert_eq!(first.version, Some(0));
    let retained = store.lease(&first)?;
    assert_eq!(retained.read(b"a")?, Some(b"one".to_vec()));
    assert_eq!(retained.read(b"missing")?, None);
    let second = store.update(&first, vec![(b"a".to_vec(), None), put(b"b", b"three")])?;
    assert_eq!(store.read(&second, b"a")?, None);
    assert_eq!(store.read(&second, b"b")?, Some(b"three".to_vec()));
    assert_eq!(retained.read(b"a")?, Some(b"one".to_vec()));
    let deleted = store.update(&second, vec![(b"b".to_vec(), None)])?;
    assert_ne!(deleted.hash, empty.hash, "authenticated queue remains after logical deletion");
    assert_eq!(deleted.version, Some(2));
    assert_eq!(store.read(&deleted, b"b")?, None);
    Ok(())
}

#[test]
fn absent_deletions_are_exact_proved_noops_even_with_another_leaf() -> Result<()> {
    let (_dir, store) = fresh()?;
    let first = store.update(&Root::empty([2; 32]), vec![put(b"present", b"value")])?;
    let second = store.update(&first, vec![(b"absent".to_vec(), None)])?;
    assert_ne!(second.hash, first.hash, "no-op still advances authenticated maintenance metadata");
    assert_eq!(second.version, Some(1));
    assert_eq!(store.read(&second, b"present")?, Some(b"value".to_vec()));
    assert_eq!(store.read(&second, b"absent")?, None);
    let third = store.update(&second, Vec::new())?;
    assert_ne!(third.hash, second.hash);
    assert_eq!(store.read(&third, b"present")?, Some(b"value".to_vec()));
    assert_eq!(store.read(&third, b"absent")?, None);
    assert_eq!(third.version, Some(2));
    Ok(())
}

#[test]
fn order_is_deterministic_and_duplicate_keys_refuse_without_writes() -> Result<()> {
    let (dir_a, a) = fresh()?;
    let (_dir_b, b) = fresh()?;
    let empty = Root::empty([3; 32]);
    let first = a.update(&empty, vec![put(b"a", b"1"), put(b"b", b"2")])?;
    let second = b.update(&empty, vec![put(b"b", b"2"), put(b"a", b"1")])?;
    assert_eq!(first, second);
    let count = row_count(&raw(&dir_a)?, "nodes")?;
    assert!(a.update(&first, vec![(b"a".to_vec(), None), put(b"a", b"3")]).is_err());
    assert_eq!(row_count(&raw(&dir_a)?, "nodes")?, count);
    assert_eq!(a.read(&first, b"a")?, Some(b"1".to_vec()));
    Ok(())
}

#[test]
fn nullable_root_version_is_required_and_unknown_fields_refuse() -> Result<()> {
    let root = Root::empty([4; 32]);
    let encoded = serde_json::to_value(&root)?;
    assert_eq!(serde_json::from_value::<Root>(encoded.clone())?, root);
    for field in ["namespace", "version", "hash"] {
        let mut missing = encoded.clone();
        missing.as_object_mut().expect("root object").remove(field);
        assert!(serde_json::from_value::<Root>(missing).is_err(), "{field}");
    }
    let mut extra = encoded;
    extra.as_object_mut().expect("root object").insert("other".into(), 0.into());
    assert!(serde_json::from_value::<Root>(extra).is_err());
    Ok(())
}

#[test]
fn namespaces_and_empty_bytes_remain_distinct() -> Result<()> {
    let (_dir, store) = fresh()?;
    let a = store.update(&Root::empty([5; 32]), vec![put(b"", b"")])?;
    let b = store.update(&Root::empty([6; 32]), vec![put(b"", b"other")])?;
    assert_eq!(store.read(&a, b"")?, Some(Vec::new()));
    assert_eq!(store.read(&b, b"")?, Some(b"other".to_vec()));
    let mut mixed = a.clone();
    mixed.namespace = b.namespace;
    assert!(store.read(&mixed, b"").is_err());
    Ok(())
}

#[test]
fn stale_writer_root_and_version_overflow_refuse() -> Result<()> {
    let (_dir, store) = fresh()?;
    let empty = Root::empty([7; 32]);
    let first = store.update(&empty, vec![put(b"x", b"one")])?;
    let second = store.update(&first, vec![put(b"x", b"two")])?;
    assert!(store.update(&first, vec![put(b"x", b"fork")]).is_err());
    assert_eq!(store.read(&second, b"x")?, Some(b"two".to_vec()));
    let exhausted = Root { version: Some(i64::MAX as u64), ..second };
    assert!(store.update(&exhausted, Vec::new()).is_err());
    Ok(())
}

/// Creates an isolated file-backed store without a new test dependency.
/// Error cases:
/// - temporary directory or store creation fails
pub(super) fn fresh() -> Result<(TestDir, Store)> {
    let path =
        std::env::temp_dir().join(format!("engine-authenticated-{}-{}", std::process::id(), rand::random::<u64>()));
    std::fs::create_dir(&path)?;
    let dir = TestDir { path };
    let store = Store::create(dir.path.join("store.sqlite"))?;
    Ok((dir, store))
}

/// Builds one exact test update.
pub(super) fn put(key: &[u8], value: &[u8]) -> (Vec<u8>, Option<Vec<u8>>) {
    (key.to_vec(), Some(value.to_vec()))
}

/// Opens a host-side adversarial connection, deliberately bypassing the store wrapper.
/// Error cases:
/// - the temporary database cannot be opened
pub(super) fn raw(dir: &TestDir) -> Result<Connection> {
    Ok(Connection::open(dir.path.join("store.sqlite"))?)
}

/// Leaves the store at `path` as a kill leaves it after SQLite synced a transaction's rollback
/// journal but before the transaction changed the database file: a hot journal beside the file.
pub(crate) fn interrupt_transaction(path: &Path) {
    let before = std::fs::read(path).unwrap();
    let journal = crate::authenticated::geometry::sidecar(path, "-journal");
    let conn = Connection::open(path).unwrap();
    // A small page cache spills mid-transaction, which syncs the journal and its header first.
    conn.execute_batch("PRAGMA cache_size = 1; BEGIN IMMEDIATE").unwrap();
    for n in 0..256i64 {
        conn.execute(
            "INSERT INTO nodes(namespace, version, path, data) VALUES(?1, ?2, ?3, ?4)",
            rusqlite::params![&[9u8; 32][..], 1_000_000 + n, &n.to_be_bytes()[..], &[0u8; 4096][..]],
        )
        .unwrap();
    }
    let hot = std::fs::read(&journal).unwrap();
    assert_ne!(hot.first(), Some(&0), "SQLite synced the journal header");
    conn.execute_batch("ROLLBACK").unwrap();
    drop(conn);
    std::fs::write(path, before).unwrap();
    std::fs::write(journal, hot).unwrap();
}

/// Counts test fixture rows in a fixed, test-owned table.
/// Error cases:
/// - SQLite cannot count the fixture rows
pub(super) fn row_count(conn: &Connection, table: &str) -> Result<i64> {
    assert!(matches!(table, "nodes" | "value_rows"));
    Ok(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?)
}
