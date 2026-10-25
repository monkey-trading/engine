//! Strict preflight opens must preserve every byte and directory entry, including bad-cache evidence.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Seek, SeekFrom, Write};

use anyhow::Result;
use rusqlite::params;

use crate::authenticated::receipts::receipt_key;
use crate::authenticated::tests::{TestDir, fresh, put, raw};
use crate::authenticated::{COMPLETE_RECEIPT_BUDGET, Root, Store, codec, db, geometry};

#[test]
fn preflight_open_and_proved_reads_create_no_lock_or_other_file_and_change_no_bytes() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([61; 32]), vec![put(b"state", b"exact prefix")])?;
    drop(store);
    std::fs::remove_file(dir.path.join("store.sqlite.lock"))?;
    let before = snapshot(&dir)?;
    let readonly = Store::open_existing(dir.path.join("store.sqlite"))?;
    assert_eq!(readonly.read(&root, b"state")?, Some(b"exact prefix".to_vec()));
    assert_eq!(readonly.read(&root, b"absent")?, None);
    assert!(readonly.update(&root, vec![put(b"state", b"forbidden")]).is_err());
    assert!(readonly.gc(&root, COMPLETE_RECEIPT_BUDGET).is_err());
    drop(readonly);
    assert_eq!(snapshot(&dir)?, before);
    assert!(!dir.path.join("store.sqlite.lock").exists());
    Ok(())
}

#[test]
fn wal_evidence_is_rejected_and_a_journal_is_read_past_without_recovery_or_byte_changes() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([62; 32]), vec![put(b"a", b"value")])?;
    drop(store);
    for suffix in ["-wal", "-shm"] {
        let sidecar = dir.path.join(format!("store.sqlite{suffix}"));
        std::fs::write(&sidecar, b"preserved recovery evidence")?;
        let before = snapshot(&dir)?;
        assert!(Store::open_existing(dir.path.join("store.sqlite")).is_err());
        assert_eq!(snapshot(&dir)?, before);
        std::fs::remove_file(sidecar)?;
    }
    // A hot journal (nonzero first byte) and one SQLite never synced, which it ignores.
    for journal in [b"preserved recovery evidence".to_vec(), vec![0; 512]] {
        let sidecar = dir.path.join("store.sqlite-journal");
        std::fs::write(&sidecar, journal)?;
        let before = snapshot(&dir)?;
        let readonly = Store::open_existing(dir.path.join("store.sqlite"))?;
        assert_eq!(readonly.read(&root, b"a")?, Some(b"value".to_vec()));
        drop(readonly);
        assert_eq!(snapshot(&dir)?, before);
        std::fs::remove_file(sidecar)?;
    }
    Ok(())
}

#[test]
fn an_interrupted_transaction_is_read_as_it_was_and_rolled_back_only_when_writes_are_enabled() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([67; 32]), vec![put(b"state", b"committed")])?;
    drop(store);
    std::fs::remove_file(dir.path.join("store.sqlite.lock"))?;
    let path = dir.path.join("store.sqlite");
    crate::authenticated::tests::interrupt_transaction(&path);
    let before = snapshot(&dir)?;
    let image = Store::open_existing(&path)?;
    assert_eq!(image.read(&root, b"state")?, Some(b"committed".to_vec()));
    assert!(image.update(&root, vec![put(b"state", b"forbidden")]).is_err());
    assert_eq!(snapshot(&dir)?, before, "reading a crash image changes no byte and keeps its journal");
    image.enable_writes()?;
    assert!(!dir.path.join("store.sqlite-journal").exists(), "enabling writes rolled the journal back");
    let next = image.update(&root, vec![put(b"state", b"after rollback")])?;
    assert_eq!(image.read(&next, b"state")?, Some(b"after rollback".to_vec()));
    Ok(())
}

#[test]
fn wal_appearing_after_geometry_preflight_cannot_create_or_rewrite_sidecars() -> Result<()> {
    let (dir, store) = fresh()?;
    store.update(&Root::empty([65; 32]), vec![put(b"state", b"ready")])?;
    drop(store);
    let path = dir.path.join("store.sqlite");
    geometry::check(&path)?;
    let host = raw(&dir)?;
    let _: String = host.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    host.pragma_update(None, "wal_autocheckpoint", 0)?;
    host.execute(
        "INSERT INTO nodes(namespace,version,path,data) VALUES(?1,0,?2,?3)",
        params![&[88u8; 32][..], codec::path(&codec::root_key(0)?)?, br#""Null""#.as_slice()],
    )?;
    let before = snapshot(&dir)?;
    let attempt = (|| -> Result<()> {
        let mut reader = db::connect(&path, false)?;
        let tx = reader.transaction()?;
        db::validate(&tx)?;
        tx.commit()?;
        Ok(())
    })();
    assert!(attempt.is_err(), "the raced WAL cache is unavailable");
    assert_eq!(snapshot(&dir)?, before, "readonly connection must not touch WAL/shared memory");
    Ok(())
}

#[test]
fn quiescent_wal_race_also_preserves_every_recovery_byte_on_close() -> Result<()> {
    let (dir, store) = fresh()?;
    store.update(&Root::empty([66; 32]), vec![put(b"state", b"ready")])?;
    drop(store);
    let path = dir.path.join("store.sqlite");
    geometry::check(&path)?;
    let host = raw(&dir)?;
    host.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    let _: String = host.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    host.pragma_update(None, "wal_autocheckpoint", 0)?;
    host.execute(
        "INSERT INTO nodes(namespace,version,path,data) VALUES(?1,0,?2,?3)",
        params![&[89u8; 32][..], codec::path(&codec::root_key(0)?)?, br#""Null""#.as_slice()],
    )?;
    drop(host);
    let before = snapshot(&dir)?;
    let attempt = (|| -> Result<()> {
        let mut conn = db::connect(&path, false)?;
        let tx = conn.transaction()?;
        db::validate(&tx)?;
        tx.commit()?;
        Ok(())
    })();
    assert!(attempt.is_err());
    assert_eq!(snapshot(&dir)?, before);
    Ok(())
}

#[test]
fn malformed_geometry_never_opens_a_writable_sqlite_connection() -> Result<()> {
    let (dir, store) = fresh()?;
    store.update(&Root::empty([63; 32]), vec![put(b"a", b"value")])?;
    drop(store);
    let database = dir.path.join("store.sqlite");
    let original = std::fs::read(&database)?;
    for (offset, bytes) in [(16, vec![0u8, 0]), (18, vec![2u8, 2]), (28, vec![255u8; 4]), (44, vec![0u8; 4])] {
        let mut file = std::fs::OpenOptions::new().write(true).open(&database)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&bytes)?;
        drop(file);
        let before = snapshot(&dir)?;
        assert!(Store::open_existing(&database).is_err());
        assert_eq!(snapshot(&dir)?, before);
        std::fs::write(&database, &original)?;
    }
    let mut file = std::fs::OpenOptions::new().append(true).open(&database)?;
    file.write_all(&[0])?;
    drop(file);
    let before = snapshot(&dir)?;
    assert!(Store::open_existing(&database).is_err());
    assert_eq!(snapshot(&dir)?, before);
    Ok(())
}

#[test]
fn point_readiness_ignores_unreferenced_nodes_and_defers_unread_receipt_damage() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([64; 32]), vec![put(b"state", b"ready")])?;
    let conn = raw(&dir)?;
    for i in 0u16..1024 {
        let mut namespace = [0u8; 32];
        namespace[..2].copy_from_slice(&i.to_be_bytes());
        conn.execute(
            "INSERT INTO nodes(namespace,version,path,data) VALUES(?1,0,?2,?3)",
            params![&namespace[..], codec::path(&codec::root_key(0)?)?, b"malformed unused JSON".as_slice()],
        )?;
    }
    let receipt = receipt_key(&root.namespace, 0)?;
    conn.execute(
        "UPDATE value_rows SET data=?1 WHERE key=?2",
        params![b"unread corrupt receipt".as_slice(), &receipt.0[..]],
    )?;
    drop(conn);
    drop(store);
    let readonly = Store::open_existing(dir.path.join("store.sqlite"))?;
    assert_eq!(readonly.read(&root, b"state")?, Some(b"ready".to_vec()));
    assert_eq!(readonly.read(&root, b"absent")?, None);
    readonly.enable_writes()?;
    assert!(readonly.gc(&root, COMPLETE_RECEIPT_BUDGET).is_err());
    assert_eq!(readonly.read(&root, b"state")?, Some(b"ready".to_vec()));
    Ok(())
}

/// Captures actual fixture bytes and directory entries, rather than trusting a successful open result.
/// Error cases:
/// - the test directory or one of its files cannot be read
fn snapshot(dir: &TestDir) -> Result<BTreeMap<OsString, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(&dir.path)? {
        let entry = entry?;
        files.insert(entry.file_name(), std::fs::read(entry.path())?);
    }
    Ok(files)
}
