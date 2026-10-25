//! Host SQL tampering must produce failure before successful data or root certification.

use std::panic::{AssertUnwindSafe, catch_unwind};

use anyhow::Result;
use rusqlite::params;

use crate::authenticated::tests::{fresh, put, raw, row_count};
use crate::authenticated::{MAX_NODE_BYTES, MAX_STORED_VALUE, Root, codec, key_hash};

#[test]
fn exact_value_tampering_and_omission_cannot_become_absence() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([11; 32]), vec![put(b"a", b"trusted")])?;
    let conn = raw(&dir)?;
    conn.execute("UPDATE value_rows SET data=?1", [b"forged".as_slice()])?;
    assert!(store.read(&root, b"a").is_err());
    conn.execute("DELETE FROM value_rows", [])?;
    assert!(store.read(&root, b"a").is_err());
    assert_eq!(store.read(&root, b"missing")?, None);
    Ok(())
}

#[test]
fn a_forged_empty_root_node_never_proves_nonmembership() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([12; 32]), vec![put(b"a", b"trusted")])?;
    raw(&dir)?.execute("UPDATE nodes SET data=?1", [br#""Null""#.as_slice()])?;
    assert!(store.read(&root, b"a").is_err());
    assert!(store.read(&root, b"missing").is_err());
    assert!(store.update(&root, vec![put(b"a", b"next")]).is_err());
    Ok(())
}

#[test]
fn altered_untouched_sibling_cannot_enter_a_new_certified_root() -> Result<()> {
    let (dir, store) = fresh()?;
    let namespace = [13; 32];
    let (a, b) = distinct_nibbles(namespace)?;
    let root = store.update(&Root::empty(namespace), vec![put(&a, b"A"), put(&b, b"B")])?;
    let conn = raw(&dir)?;
    let before = row_count(&conn, "nodes")?;
    let target_hash = key_hash(&namespace, &b)?;
    let root_key = codec::root_key(0)?;
    let root_raw: Vec<u8> =
        conn.query_row("SELECT data FROM nodes WHERE version=0 AND path=?1", [codec::path(&root_key)?], |r| r.get(0))?;
    let root_wire: serde_json::Value = serde_json::from_slice(&root_raw)?;
    let nibble = target_hash.0[0] >> 4;
    let child_version =
        root_wire["Internal"]["children"]["children"][nibble as usize]["version"].as_u64().expect("root child");
    let child_path = codec::path(&codec::child_key(&root_key, nibble, child_version)?)?;
    let child_raw: Vec<u8> =
        conn.query_row("SELECT data FROM nodes WHERE version=0 AND path=?1", [&child_path], |r| r.get(0))?;
    let mut child: serde_json::Value = serde_json::from_slice(&child_raw)?;
    if child.get("Leaf").is_some() {
        child["Leaf"]["value_hash"] = serde_json::to_value([99u8; 32])?;
    } else {
        let children = child["Internal"]["children"]["children"].as_array_mut().expect("child array");
        let entry = children.iter_mut().find(|v| !v.is_null()).expect("nonempty child");
        entry["hash"] = serde_json::to_value([99u8; 32])?;
    }
    conn.execute(
        "UPDATE nodes SET data=?1 WHERE namespace=?2 AND version=0 AND path=?3",
        params![serde_json::to_vec(&child)?, &namespace[..], child_path],
    )?;
    assert!(store.update(&root, vec![put(&a, b"new A")]).is_err());
    assert_eq!(row_count(&conn, "nodes")?, before);
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM value_rows WHERE version=1", [], |r| r.get::<_, i64>(0))?, 0);
    Ok(())
}

#[test]
fn missing_siblings_and_malformed_counters_return_errors_before_upstream_panics() -> Result<()> {
    let (dir, store) = fresh()?;
    let namespace = [14; 32];
    let (a, b) = distinct_nibbles(namespace)?;
    let root = store.update(&Root::empty(namespace), vec![put(&a, b"A"), put(&b, b"B")])?;
    let conn = raw(&dir)?;
    let root_path = codec::path(&codec::root_key(0)?)?;
    let original: Vec<u8> = conn.query_row("SELECT data FROM nodes WHERE path=?1", [&root_path], |r| r.get(0))?;
    let mut wire: serde_json::Value = serde_json::from_slice(&original)?;
    wire["Internal"]["children"]["num_children"] = 0.into();
    conn.execute("UPDATE nodes SET data=?1 WHERE path=?2", params![serde_json::to_vec(&wire)?, &root_path])?;
    assert!(
        catch_unwind(AssertUnwindSafe(|| store.read(&root, &a))).expect("untrusted counters must not panic").is_err()
    );
    conn.execute("UPDATE nodes SET data=?1 WHERE path=?2", params![original, &root_path])?;
    conn.execute("DELETE FROM nodes WHERE path<>?1", [&root_path])?;
    assert!(catch_unwind(AssertUnwindSafe(|| store.read(&root, &a))).expect("missing sibling must not panic").is_err());
    assert!(
        catch_unwind(AssertUnwindSafe(|| store.update(&root, vec![put(&a, b"next")])))
            .expect("missing sibling must not panic on update")
            .is_err()
    );
    Ok(())
}

#[test]
fn schema_is_rechecked_on_each_read_write_and_gc() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([15; 32]), vec![put(b"a", b"A")])?;
    let conn = raw(&dir)?;
    conn.execute_batch("CREATE TRIGGER injected AFTER INSERT ON value_rows BEGIN DELETE FROM nodes; END;")?;
    let before = row_count(&conn, "nodes")?;
    assert!(store.read(&root, b"a").is_err());
    assert!(store.update(&root, vec![put(b"a", b"B")]).is_err());
    assert!(store.gc(&root, 1).is_err());
    assert_eq!(row_count(&conn, "nodes")?, before);
    conn.execute_batch("DROP TRIGGER injected")?;
    assert_eq!(store.read(&root, b"a")?, Some(b"A".to_vec()));
    Ok(())
}

#[test]
fn malformed_lengths_versions_and_leaf_counters_never_reach_upstream_assertions() -> Result<()> {
    let (dir, store) = fresh()?;
    let namespace = [17; 32];
    let (a, b) = distinct_nibbles(namespace)?;
    let root = store.update(&Root::empty(namespace), vec![put(&a, b"A"), put(&b, b"B")])?;
    let conn = raw(&dir)?;
    let path = codec::path(&codec::root_key(0)?)?;
    let original: Vec<u8> = conn.query_row("SELECT data FROM nodes WHERE path=?1", [&path], |r| r.get(0))?;
    let original: serde_json::Value = serde_json::from_slice(&original)?;
    let index = original["Internal"]["children"]["children"]
        .as_array()
        .expect("fixed child array")
        .iter()
        .position(|child| !child.is_null())
        .expect("fixture child");
    let mut variants = Vec::new();
    let mut future = original.clone();
    future["Internal"]["children"]["children"][index]["version"] = u64::MAX.into();
    variants.push(future);
    let mut overflow = original.clone();
    overflow["Internal"]["leaf_count"] = u64::MAX.into();
    variants.push(overflow);
    let mut arity = original;
    arity["Internal"]["children"]["children"].as_array_mut().expect("fixed child array").push(serde_json::Value::Null);
    variants.push(arity);
    for malformed in variants {
        conn.execute("UPDATE nodes SET data=?1 WHERE path=?2", params![serde_json::to_vec(&malformed)?, &path])?;
        assert!(
            catch_unwind(AssertUnwindSafe(|| store.read(&root, &a)))
                .expect("malformed disk fields return an error")
                .is_err()
        );
        assert!(
            catch_unwind(AssertUnwindSafe(|| store.update(&root, vec![put(&a, b"next")])))
                .expect("malformed disk fields cannot panic during update")
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn host_lengths_cannot_request_unbounded_node_or_value_decoding() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([16; 32]), vec![put(b"a", b"A")])?;
    let conn = raw(&dir)?;
    conn.pragma_update(None, "ignore_check_constraints", true)?;
    conn.execute("UPDATE value_rows SET data=zeroblob(?1)", [(MAX_STORED_VALUE + 1) as i64])?;
    assert!(store.read(&root, b"a").is_err());
    conn.execute("UPDATE nodes SET data=zeroblob(?1)", [(MAX_NODE_BYTES + 1) as i64])?;
    assert!(store.read(&root, b"a").is_err());
    Ok(())
}

/// Finds distinct first nibbles so the fixture has two independently stored root children.
/// Error cases:
/// - key hashing fails; the finite fixture search failing is a test failure
fn distinct_nibbles(namespace: [u8; 32]) -> Result<(Vec<u8>, Vec<u8>)> {
    let a = b"first".to_vec();
    let first = key_hash(&namespace, &a)?.0[0] >> 4;
    for i in 0u32..1024 {
        let b = i.to_be_bytes().to_vec();
        if key_hash(&namespace, &b)?.0[0] >> 4 != first {
            return Ok((a, b));
        }
    }
    panic!("fixed namespace must yield distinct nibbles in the fixture search");
}
