//! Garbage collection across restarts, forged receipts, retained pins, and storage reclamation.

use anyhow::Result;
use rusqlite::params;

use crate::authenticated::receipts::{queue, receipt_key};
use crate::authenticated::tests::{fresh, put, raw, row_count};
use crate::authenticated::{COMPLETE_RECEIPT_BUDGET, Root, Store};

#[test]
fn authenticated_receipts_reclaim_after_every_reopen_and_bound_residual_storage() -> Result<()> {
    let (dir, initial) = fresh()?;
    let mut store = initial;
    let mut root = Root::empty([41; 32]);
    let mut baseline = None;
    let mut baseline_pages = None;
    for round in 0u8..32 {
        root = store.update(&root, vec![put(b"a", &vec![round; 65543])])?;
        drop(store);
        store = Store::open_existing(dir.path.join("store.sqlite"))?;
        store.enable_writes()?;
        let (collected, report) = store.gc(&root, COMPLETE_RECEIPT_BUDGET)?;
        if round != 0 {
            assert!(report.deleted > 0, "restart retained authenticated deletion authority");
        }
        root = collected;
        let (same, idle) = store.gc(&root, COMPLETE_RECEIPT_BUDGET)?;
        assert!(idle.idle, "maintenance reaches its bounded terminal receipt");
        assert_eq!(same, root);
        assert_eq!(store.read(&root, b"a")?, Some(vec![round; 65543]));
        let conn = raw(&dir)?;
        let rows = row_count(&conn, "nodes")? + row_count(&conn, "value_rows")?;
        let pages: i64 = conn.pragma_query_value(None, "page_count", |r| r.get(0))?;
        let first = *baseline.get_or_insert(rows);
        let first_pages = *baseline_pages.get_or_insert(pages);
        assert!(rows <= first + 256, "garbage cannot accumulate once per restart: {rows}");
        assert!(pages <= first_pages + 64, "freed pages are reused/reclaimed: {pages}");
    }
    Ok(())
}

#[test]
fn forged_receipt_cannot_authorize_a_live_row_deletion() -> Result<()> {
    let (dir, store) = fresh()?;
    let first = store.update(&Root::empty([42; 32]), vec![put(b"a", b"old")])?;
    let root = store.update(&first, vec![put(b"a", b"live")])?;
    let conn = raw(&dir)?;
    let key = receipt_key(&root.namespace, 1)?;
    let original: Vec<u8> = conn.query_row("SELECT data FROM value_rows WHERE key=?1", [&key.0[..]], |r| r.get(0))?;
    let mut forged: serde_json::Value = serde_json::from_slice(&original)?;
    forged["rows"] = serde_json::json!([{"Node":{"version":1,"nibble_path":{"num_nibbles":0,"bytes":[]}}}]);
    conn.execute("UPDATE value_rows SET data=?1 WHERE key=?2", params![serde_json::to_vec(&forged)?, &key.0[..]])?;
    let before = row_count(&conn, "nodes")?;
    assert!(store.gc(&root, COMPLETE_RECEIPT_BUDGET).is_err());
    assert_eq!(row_count(&conn, "nodes")?, before);
    assert_eq!(store.read(&root, b"a")?, Some(b"live".to_vec()));
    Ok(())
}

#[test]
fn valid_receipt_from_another_fork_in_the_same_namespace_is_rejected() -> Result<()> {
    let (a_dir, a) = fresh()?;
    let (b_dir, b) = fresh()?;
    let empty = Root::empty([43; 32]);
    let a0 = a.update(&empty, vec![put(b"a", b"A")])?;
    let a1 = a.update(&a0, vec![put(b"a", b"AA")])?;
    let b0 = b.update(&empty, vec![put(b"a", b"B")])?;
    let b1 = b.update(&b0, vec![put(b"a", b"BB")])?;
    assert_ne!(a1.hash, b1.hash);
    let key = receipt_key(&a1.namespace, 1)?;
    let from_fork: Vec<u8> =
        raw(&b_dir)?.query_row("SELECT data FROM value_rows WHERE key=?1", [&key.0[..]], |r| r.get(0))?;
    let conn = raw(&a_dir)?;
    conn.execute("UPDATE value_rows SET data=?1 WHERE key=?2", params![from_fork, &key.0[..]])?;
    let before = row_count(&conn, "nodes")?;
    assert!(a.gc(&a1, COMPLETE_RECEIPT_BUDGET).is_err());
    assert_eq!(row_count(&conn, "nodes")?, before);
    assert_eq!(a.read(&a1, b"a")?, Some(b"AA".to_vec()));
    Ok(())
}

#[test]
fn more_than_4096_retained_roots_have_no_arbitrary_lease_cap() -> Result<()> {
    let (_dir, store) = fresh()?;
    let old = store.update(&Root::empty([44; 32]), vec![put(b"a", b"old")])?;
    let retained = store.lease(&old)?;
    let roots: Vec<_> = (0u64..5000)
        .map(|index| {
            let mut namespace = [99; 32];
            namespace[..8].copy_from_slice(&index.to_be_bytes());
            Root::empty(namespace)
        })
        .collect();
    let pins = store.pin_versions(&roots)?;
    assert_eq!(pins.len(), 5000);
    let root = store.update(&old, vec![put(b"a", b"current")])?;
    let (unchanged, blocked) = store.gc(&root, COMPLETE_RECEIPT_BUDGET)?;
    assert_eq!(unchanged, root);
    assert!(blocked.blocked_by_pins);
    assert_eq!(retained.read(b"a")?, Some(b"old".to_vec()));
    drop(retained);
    let (new, report) = store.gc(&root, COMPLETE_RECEIPT_BUDGET)?;
    assert!(report.deleted > 0);
    assert_eq!(store.read(&new, b"a")?, Some(b"current".to_vec()));
    assert_eq!(pins[4999].read(b"anything")?, None);
    Ok(())
}

#[test]
fn small_budget_defers_whole_receipts_without_truncating_collection() -> Result<()> {
    let (dir, store) = fresh()?;
    let first = store.update(&Root::empty([45; 32]), vec![put(b"a", b"old")])?;
    let root = store.update(&first, vec![put(b"a", b"current")])?;
    let before = row_count(&raw(&dir)?, "nodes")?;
    let (same, small) = store.gc(&root, 1)?;
    assert_eq!(same, root);
    assert_eq!(small.deleted, 0);
    assert!(small.minimum_budget > 1);
    assert_eq!(row_count(&raw(&dir)?, "nodes")?, before);
    let (next, report) = store.gc(&root, COMPLETE_RECEIPT_BUDGET)?;
    assert!(report.deleted > 0 && report.deleted <= COMPLETE_RECEIPT_BUDGET);
    assert_ne!(next, root);
    assert!(store.update(&root, vec![put(b"a", b"stale")]).is_err());
    assert_eq!(store.read(&next, b"a")?, Some(b"current".to_vec()));
    let conn = raw(&dir)?;
    let q = queue(&conn, &next)?;
    assert_eq!(q.hi - q.lo, 1);
    let (idle_root, idle) = store.gc(&next, COMPLETE_RECEIPT_BUDGET)?;
    assert!(idle.idle);
    assert_eq!(idle_root, next);
    Ok(())
}

#[test]
fn retained_certificate_pins_restored_after_restart_protect_their_exact_version() -> Result<()> {
    let (dir, store) = fresh()?;
    let retained = store.update(&Root::empty([46; 32]), vec![put(b"a", b"retained")])?;
    let current = store.update(&retained, vec![put(b"a", b"current")])?;
    drop(store);
    let reopened = Store::open_existing(dir.path.join("store.sqlite"))?;
    let pins = reopened.pin_versions(std::slice::from_ref(&retained))?;
    reopened.enable_writes()?;
    let (same, blocked) = reopened.gc(&current, COMPLETE_RECEIPT_BUDGET)?;
    assert_eq!(same, current);
    assert!(blocked.blocked_by_pins);
    assert_eq!(pins[0].read(b"a")?, Some(b"retained".to_vec()));
    drop(pins);
    let (advanced, report) = reopened.gc(&current, COMPLETE_RECEIPT_BUDGET)?;
    assert!(report.deleted > 0);
    assert_eq!(reopened.read(&advanced, b"a")?, Some(b"current".to_vec()));
    assert!(reopened.read(&retained, b"a").is_err());
    Ok(())
}

#[test]
fn interrupted_gc_rolls_back_receipt_consumption_and_physical_deletion_together() -> Result<()> {
    let (dir, store) = fresh()?;
    let first = store.update(&Root::empty([47; 32]), vec![put(b"a", b"old")])?;
    let root = store.update(&first, vec![put(b"a", b"current")])?;
    let conn = raw(&dir)?;
    let before_nodes = row_count(&conn, "nodes")?;
    let before_values = row_count(&conn, "value_rows")?;
    let before_cursor = queue(&conn, &root)?.lo;
    let fail =
        store.gc_checked(&root, COMPLETE_RECEIPT_BUDGET, |_| anyhow::bail!("interrupt GC before commit"), || Ok(()));
    assert!(fail.is_err());
    assert_eq!(row_count(&conn, "nodes")?, before_nodes);
    assert_eq!(row_count(&conn, "value_rows")?, before_values);
    assert_eq!(queue(&conn, &root)?.lo, before_cursor);
    assert_eq!(store.read(&root, b"a")?, Some(b"current".to_vec()));
    assert_eq!(store.read(&first, b"a")?, Some(b"old".to_vec()), "rolled-back GC must restore lease admission too");
    drop(conn);
    drop(store);
    let reopened = Store::open_existing(dir.path.join("store.sqlite"))?;
    reopened.enable_writes()?;
    let (new, report) = reopened.gc(&root, COMPLETE_RECEIPT_BUDGET)?;
    assert!(report.deleted > 0);
    assert_eq!(reopened.read(&new, b"a")?, Some(b"current".to_vec()));
    Ok(())
}
