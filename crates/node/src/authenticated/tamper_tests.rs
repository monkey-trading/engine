//! Host SQL tampering must produce failure before successful data or root certification.

use std::panic::{AssertUnwindSafe, catch_unwind};

use anyhow::Result;
use rusqlite::params;

use crate::authenticated::tests::{fresh, put, raw, row_count};
use crate::authenticated::{MAX_NODE_BYTES, MAX_STORED_VALUE, Root, caller_key, codec, key_hash};

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

/// A value stored whole in its leaf is read only as the root proves it: a forged one, a forged
/// descriptor in its place, and an omitted one all fail.
#[test]
fn a_forged_value_stored_whole_cannot_be_read() -> Result<()> {
    let (dir, store) = fresh()?;
    store.write_compactly([14; 32])?;
    let root = store.update(&Root::empty([14; 32]), vec![put(b"a", b"trusted")])?;
    let conn = raw(&dir)?;
    let key = caller_key(&[14; 32], b"a")?.0;
    let stored =
        conn.query_row("SELECT data FROM value_rows WHERE key=?1", [&key[..]], |row| row.get::<_, Vec<u8>>(0))?;
    assert_eq!(stored, b"\0trusted", "the value is its leaf");
    let forge = |data: &[u8]| conn.execute("UPDATE value_rows SET data=?1 WHERE key=?2", params![data, &key[..]]);
    forge(b"\0forged!")?;
    assert!(store.read(&root, b"a").is_err(), "a forged value stored whole was read");
    forge(format!(r#"{{"length":7,"digest":{:?}}}"#, [0u8; 32]).as_bytes())?;
    assert!(store.read(&root, b"a").is_err(), "a forged descriptor was read");
    conn.execute("DELETE FROM value_rows WHERE key=?1", [&key[..]])?;
    assert!(store.read(&root, b"a").is_err(), "an omitted value stored whole became absence");
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
        "UPDATE nodes SET data=?1 WHERE space=(SELECT id FROM spaces WHERE namespace=?2) AND version=0 AND path=?3",
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

/// Damage written into the file's bytes between two writes, with SQLite's header and change counter left as
/// they were, is refused by the next write that reads the damaged node: each write opens its own connection,
/// which reads its pages and schema from the file.
#[test]
fn raw_damage_that_keeps_the_header_is_refused_by_the_next_write() -> Result<()> {
    let (dir, store) = fresh()?;
    // Compact, so a byte of a child's hash can change and the node still decode.
    store.write_compactly([19; 32])?;
    let root = store.update(&Root::empty([19; 32]), vec![put(b"a", b"A"), put(b"b", b"B")])?;
    let key = codec::root_key(root.version.expect("a version"))?;
    let node: Vec<u8> =
        raw(&dir)?.query_row("SELECT data FROM nodes WHERE path = ?1", [codec::path(&key)?], |row| row.get(0))?;
    let child_hash = 2 + node[1..].iter().position(|tag| *tag == 1).expect("a child");
    let path = dir.path.join("store.sqlite");
    let mut file = std::fs::read(&path)?;
    let header = file[..100].to_vec();
    let at = file.windows(node.len()).position(|window| window == node.as_slice()).expect("the root node's bytes");
    file[at + child_hash + 5] ^= 0x55;
    std::fs::write(&path, &file)?;
    assert_eq!(std::fs::read(&path)?[..100], header, "the header is unchanged");
    let refused = store.update(&root, vec![put(b"a", b"next")]).expect_err("the damaged root node is refused");
    assert!(format!("{refused:#}").contains("root node hash mismatch"), "{refused:#}");
    Ok(())
}

/// Swapping the root pages of the `nodes` and `value_rows` tables in the file's schema between two writes,
/// with SQLite's header and schema cookie left as they were, is refused by the next write, which reads the
/// schema from the file, and the write leaves the file as it found it.
#[test]
fn a_schema_damaged_below_the_header_is_refused_by_the_next_write() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([20; 32]), vec![put(b"a", b"A"), put(b"b", b"B")])?;
    let path = dir.path.join("store.sqlite");
    let original = std::fs::read(&path)?;
    // A `sqlite_schema` record's body holds its type, name and table name side by side, then its root page,
    // one byte for a page below 128, then its SQL.
    let root_page_at = |table: &str| -> usize {
        let record = [b"table".as_slice(), table.as_bytes(), table.as_bytes()].concat();
        let at = original.windows(record.len()).position(|window| window == record.as_slice()).expect("the record");
        assert!(original[at + record.len() + 1..].starts_with(format!("CREATE TABLE {table}").as_bytes()));
        at + record.len()
    };
    let mut damaged = original.clone();
    damaged.swap(root_page_at("nodes"), root_page_at("value_rows"));
    assert_ne!(damaged, original, "the root pages differ");
    std::fs::write(&path, &damaged)?;
    assert!(store.update(&root, vec![put(b"a", b"next")]).is_err(), "the swapped tables are refused");
    assert_eq!(std::fs::read(&path)?, damaged, "the refused write changed the file");
    assert!(!dir.path.join("store.sqlite-journal").exists(), "the refused write left a journal");
    std::fs::write(&path, &original)?;
    let next = store.update(&root, vec![put(b"a", b"next")])?;
    assert_eq!(store.read(&next, b"a")?, Some(b"next".to_vec()), "the repaired store writes from the same head");
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
