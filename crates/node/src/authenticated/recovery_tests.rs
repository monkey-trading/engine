//! Atomic rollback, durable reopen, discarding unsigned versions, lease cancellation, and collection.

use std::sync::mpsc;

use anyhow::Result;
use rusqlite::params;

use crate::authenticated::tests::{fresh, put, raw, row_count};
use crate::authenticated::{CHUNK_BYTES, INTERNAL, Root, Store, codec, key_hash};

#[test]
fn interrupted_batch_rolls_back_and_reopens_at_the_old_root() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([21; 32]), vec![put(b"a", b"old")])?;
    let before = row_count(&raw(&dir)?, "nodes")?;
    let interrupted = store.update_checked(
        &root,
        vec![put(b"a", b"uncommitted"), put(b"b", b"uncommitted")],
        |tx| {
            let new_values: i64 = tx.query_row(
                "SELECT COUNT(*) FROM value_rows WHERE version=1 AND key IN (?1,?2)",
                params![&key_hash(&root.namespace, b"a")?.0[..], &key_hash(&root.namespace, b"b")?.0[..]],
                |r| r.get(0),
            )?;
            assert_eq!(new_values, 2, "failure occurs after the complete SQL batch");
            anyhow::bail!("injected interruption before commit")
        },
        || Ok(()),
    );
    assert!(interrupted.is_err());
    assert_eq!(row_count(&raw(&dir)?, "nodes")?, before);
    assert_eq!(store.read(&root, b"a")?, Some(b"old".to_vec()));
    drop(store);
    let reopened = Store::open_existing(dir.path.join("store.sqlite"))?;
    reopened.enable_writes()?;
    assert_eq!(reopened.read(&root, b"a")?, Some(b"old".to_vec()));
    assert_eq!(reopened.read(&root, b"b")?, None);
    let next = reopened.update(&root, vec![put(b"a", b"committed")])?;
    assert_eq!(next.version, Some(1));
    Ok(())
}

#[test]
fn failure_after_commit_does_not_acknowledge_or_overwrite_the_old_root() -> Result<()> {
    let (dir, store) = fresh()?;
    let old = store.update(&Root::empty([30; 32]), vec![put(b"a", b"old")])?;
    let failed = store.update_checked(
        &old,
        vec![put(b"a", b"unacknowledged")],
        |_| Ok(()),
        || anyhow::bail!("injected failure at the post-commit durability boundary"),
    );
    assert!(failed.is_err());
    assert_eq!(
        raw(&dir)?.query_row(
            "SELECT COUNT(*) FROM value_rows WHERE version=1 AND key=?1",
            [&key_hash(&old.namespace, b"a")?.0[..]],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    assert_eq!(store.read(&old, b"a")?, Some(b"old".to_vec()));
    assert!(store.update(&old, vec![put(b"a", b"retry")]).is_err());
    assert!(store.gc(&old, 1).is_err());
    drop(store);
    let reopened = Store::open_existing(dir.path.join("store.sqlite"))?;
    reopened.enable_writes()?;
    assert_eq!(reopened.read(&old, b"a")?, Some(b"old".to_vec()));
    assert!(reopened.update(&old, vec![put(b"a", b"overwrite")]).is_err(), "version collision preserves evidence");
    assert_eq!(reopened.read(&old, b"a")?, Some(b"old".to_vec()));
    Ok(())
}

/// Versions committed after the signed root are discarded only once writes are authorized, every node
/// and value row after it with them, a chunked value's and a value stored whole included; writes then
/// resume at the next version and the signed root's values are unchanged.
#[test]
fn discarding_unsigned_versions_resumes_writes_from_the_signed_root() -> Result<()> {
    let (dir, store) = fresh()?;
    let signed = store.update(&Root::empty([24; 32]), vec![put(b"a", b"signed")])?;
    let unsigned = store.update(&signed, vec![put(b"a", b"unsigned"), put(b"b", b"unsigned")])?;
    let chunked = store.update(&unsigned, vec![put(b"c", &vec![7; CHUNK_BYTES * 2 + 1])])?;
    store.write_compactly([24; 32])?;
    store.update(&chunked, vec![put(b"d", b"whole"), (b"b".to_vec(), None)])?;
    drop(store);
    let later = |table: &str| -> Result<i64> {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE version > ?1");
        Ok(raw(&dir)?.query_row(&sql, [signed.version.map_or(-1, |v| v as i64)], |row| row.get(0))?)
    };
    assert!(later("nodes")? > 0 && later("value_rows")? > 4, "the unsigned versions hold nodes and values");
    let reopened = Store::open_existing(dir.path.join("store.sqlite"))?;
    assert!(reopened.ensure_current(&signed).is_err());
    assert!(reopened.discard_after(&signed).is_err(), "preflight stores are read-only");
    reopened.enable_writes()?;
    reopened.discard_after(&signed)?;
    assert_eq!((later("nodes")?, later("value_rows")?), (0, 0), "nothing after the signed root is left");
    reopened.ensure_current(&signed)?;
    assert_eq!(reopened.read(&signed, b"a")?, Some(b"signed".to_vec()));
    assert_eq!(reopened.read(&signed, b"b")?, None);
    let next = reopened.update(&signed, vec![put(b"b", b"resumed")])?;
    assert_eq!(next.version, signed.version.map(|v| v + 1));
    assert_eq!(reopened.read(&next, b"a")?, Some(b"signed".to_vec()));
    assert_eq!(reopened.read(&next, b"b")?, Some(b"resumed".to_vec()));
    assert_eq!(reopened.read(&next, b"c")?, None);
    Ok(())
}

/// A later node the host damaged names no key, so the discard falls back to a scan of every value and still
/// leaves nothing after the signed root, rather than refusing the start.
#[test]
fn discarding_unsigned_versions_survives_a_damaged_later_node() -> Result<()> {
    let (dir, store) = fresh()?;
    let signed = store.update(&Root::empty([23; 32]), vec![put(b"a", b"signed")])?;
    store.update(&signed, vec![put(b"a", b"unsigned"), put(b"b", b"unsigned")])?;
    drop(store);
    let after = signed.version.map_or(-1, |v| v as i64);
    raw(&dir)?.execute("UPDATE nodes SET data = x'ff' WHERE version > ?1", [after])?;
    let reopened = Store::open_existing(dir.path.join("store.sqlite"))?;
    reopened.enable_writes()?;
    reopened.discard_after(&signed)?;
    let later = |table: &str| -> Result<i64> {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE version > ?1");
        Ok(raw(&dir)?.query_row(&sql, [after], |row| row.get(0))?)
    };
    assert_eq!((later("nodes")?, later("value_rows")?), (0, 0), "nothing after the signed root is left");
    assert_eq!(reopened.read(&signed, b"a")?, Some(b"signed".to_vec()));
    let next = reopened.update(&signed, vec![put(b"b", b"resumed")])?;
    assert_eq!(reopened.read(&next, b"b")?, Some(b"resumed".to_vec()));
    Ok(())
}

/// Restart readiness must not grow with retained values: a clean restart leaves the store
/// untouched, and a crash restart reads only the later versions' nodes, by range on their key, and
/// deletes each later value by its own key.
#[test]
fn discarding_unsigned_versions_never_scans_retained_values() -> Result<()> {
    let (dir, store) = fresh()?;
    let mut signed = Root::empty([25; 32]);
    for n in 0..8u8 {
        signed = store.update(&signed, vec![put(&[n], b"retained")])?;
    }
    drop(store);
    let path = dir.path.join("store.sqlite");
    let before = std::fs::read(&path)?;
    let reopened = Store::open_existing(&path)?;
    reopened.enable_writes()?;
    reopened.discard_after(&signed)?;
    assert_eq!(std::fs::read(&path)?, before, "nothing follows the signed root, so nothing is written");
    let conn = raw(&dir)?;
    let plan = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> Result<Vec<String>> {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        Ok(statement.query_map(params, |row| row.get(3))?.collect::<rusqlite::Result<_>>()?)
    };
    let nodes = plan("SELECT data FROM nodes WHERE space = ?1 AND version > ?2", &[&1, &7])?;
    assert!(nodes.iter().all(|step| step.starts_with("SEARCH nodes USING PRIMARY KEY")), "{nodes:?}");
    let values = plan(
        "DELETE FROM value_rows WHERE space = ?1 AND key = ?2 AND version > ?3",
        &[&1, &[0u8; 32].as_slice(), &7],
    )?;
    assert!(values.iter().all(|step| step.starts_with("SEARCH value_rows USING PRIMARY KEY")), "{values:?}");
    Ok(())
}

#[test]
fn committed_values_survive_reopen_without_discovering_roots_from_sql() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([22; 32]), vec![put(b"a", b"durable")])?;
    let expected = serde_json::to_vec(&root)?;
    drop(store);
    let reopened = Store::open_existing(dir.path.join("store.sqlite"))?;
    reopened.enable_writes()?;
    let supplied: Root = serde_json::from_slice(&expected)?;
    assert_eq!(reopened.read(&supplied, b"a")?, Some(b"durable".to_vec()));
    assert_eq!(reopened.read(&supplied, b"absent")?, None);
    Ok(())
}

#[test]
fn cloned_lease_protects_the_blocking_reader_after_its_waiter_is_dropped() -> Result<()> {
    let (_dir, store) = fresh()?;
    let old = store.update(&Root::empty([23; 32]), vec![put(b"a", b"old")])?;
    let waiter = store.lease(&old)?;
    let blocking_lease = waiter.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(0);
    let (read_tx, read_rx) = mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || -> Result<_> {
        ready_tx.send(())?;
        read_rx.recv()?;
        blocking_lease.read(b"a")
    });
    ready_rx.recv()?;
    drop(waiter);
    let current = store.update(&old, vec![put(b"a", b"current")])?;
    assert_eq!(store.gc(&current, 256)?.1.deleted, 0, "worker still pins the old version");
    read_tx.send(())?;
    assert_eq!(worker.join().expect("blocking reader completes")?, Some(b"old".to_vec()));
    assert!(store.gc(&current, 256)?.1.deleted > 0);
    assert!(store.read(&old, b"a").is_err());
    assert_eq!(store.read(&current, b"a")?, Some(b"current".to_vec()));
    Ok(())
}

#[test]
fn retained_snapshot_pins_block_collection_and_each_gc_call_has_a_fixed_row_budget() -> Result<()> {
    let (_dir, store) = fresh()?;
    let old = store.update(&Root::empty([24; 32]), vec![put(b"a", b"old")])?;
    let pins = store.pin_versions(std::slice::from_ref(&old))?;
    let middle = store.update(&old, vec![put(b"a", b"middle")])?;
    let current = store.update(&middle, vec![put(b"a", b"current")])?;
    assert_eq!(store.gc(&current, 0)?.1.deleted, 0);
    assert_eq!(store.gc(&current, 256)?.1.deleted, 0);
    assert_eq!(pins[0].read(b"a")?, Some(b"old".to_vec()));
    drop(pins);
    assert!(store.gc(&current, 1)?.1.deleted <= 1);
    assert_eq!(store.read(&current, b"a")?, Some(b"current".to_vec()));
    Ok(())
}

#[test]
fn reopening_never_trusts_retirement_authority_reconstructed_from_sql() -> Result<()> {
    let (dir, store) = fresh()?;
    let old = store.update(&Root::empty([25; 32]), vec![put(b"a", b"old")])?;
    let current = store.update(&old, vec![put(b"a", b"current")])?;
    drop(store);
    let reopened = Store::open_existing(dir.path.join("store.sqlite"))?;
    reopened.enable_writes()?;
    let before = row_count(&raw(&dir)?, "nodes")?;
    let conn = raw(&dir)?;
    let receipt_key = crate::authenticated::receipts::receipt_key(&current.namespace, 0)?;
    conn.execute(
        "UPDATE value_rows SET data=?1 WHERE key=?2",
        params![b"unsigned retirement authority".as_slice(), &receipt_key.0[..]],
    )?;
    assert!(reopened.gc(&current, usize::MAX).is_err());
    assert_eq!(row_count(&raw(&dir)?, "nodes")?, before);
    assert_eq!(reopened.read(&old, b"a")?, Some(b"old".to_vec()));
    assert_eq!(reopened.read(&current, b"a")?, Some(b"current".to_vec()));
    Ok(())
}

#[test]
fn unrelated_malformed_rows_are_not_a_startup_or_point_read_scan() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([26; 32]), vec![put(b"a", b"valid")])?;
    let conn = raw(&dir)?;
    for i in 0u16..512 {
        let mut namespace = [0; 32];
        namespace[..2].copy_from_slice(&i.to_be_bytes());
        conn.execute("INSERT INTO spaces(namespace, id) VALUES(?1, ?2)", params![&namespace[..], i64::from(i) + 2])?;
        conn.execute(
            "INSERT INTO nodes(space,version,path,data) VALUES(?1,0,?2,?3)",
            params![i64::from(i) + 2, codec::path(&codec::root_key(0)?)?, b"invalid node JSON".as_slice()],
        )?;
    }
    drop(conn);
    drop(store);
    let reopened = Store::open_existing(dir.path.join("store.sqlite"))?;
    reopened.enable_writes()?;
    assert_eq!(reopened.read(&root, b"a")?, Some(b"valid".to_vec()));
    assert_eq!(reopened.read(&root, b"missing")?, None);
    Ok(())
}

#[test]
fn input_envelopes_refuse_before_changing_storage() -> Result<()> {
    let (dir, store) = fresh()?;
    let empty = Root::empty([27; 32]);
    assert!(store.read(&empty, INTERNAL).is_err());
    assert!(store.update(&empty, vec![(INTERNAL.to_vec(), Some(vec![0; 1]))]).is_err());
    assert!(store.update(&empty, vec![put(b"duplicate", b"first"), put(b"duplicate", b"second")]).is_err());
    assert_eq!(row_count(&raw(&dir)?, "nodes")?, 0);
    assert_eq!(row_count(&raw(&dir)?, "value_rows")?, 0);
    Ok(())
}

#[test]
fn former_key_and_value_limits_are_supported_as_exact_bytes() -> Result<()> {
    let (_dir, store) = fresh()?;
    let key = vec![0xabu8; 4096];
    let value = vec![0xcdu8; 1024 * 1024];
    let root = store.update(&Root::empty([29; 32]), vec![(key.clone(), Some(value.clone()))])?;
    assert_eq!(store.read(&root, &key)?, Some(value));
    Ok(())
}

#[test]
fn create_is_exclusive_and_open_existing_never_creates_a_database() -> Result<()> {
    let (dir, store) = fresh()?;
    assert!(Store::create(dir.path.join("store.sqlite")).is_err());
    let readonly = Store::open_existing(dir.path.join("store.sqlite"))?;
    assert!(readonly.enable_writes().is_err(), "one writable store owner per database");
    assert!(Store::open_existing(dir.path.join("missing.sqlite")).is_err());
    assert!(!dir.path.join("missing.sqlite").exists());
    let root = store.update(&Root::empty([28; 32]), vec![put(b"a", b"A")])?;
    assert_eq!(store.read(&root, b"a")?, Some(b"A".to_vec()));
    Ok(())
}
