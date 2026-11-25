//! The node codec gives what upstream's own decoding of a stored node gives, from its JSON or its compact
//! borsh form, parsing it once.

use anyhow::Result;
use jmt::storage::{Node, NodeKey};

use crate::authenticated::blobs::{Points, chunk_key};
use crate::authenticated::reader::Checked;
use crate::authenticated::tests::{fresh, put, raw};
use crate::authenticated::{CHUNK_BYTES, Root, caller_key, codec};

/// Upstream's tags for its node variants in its borsh form, which a compact node starts with.
const COMPACT_TAGS: [u8; 3] = [0, 1, 2];

/// Every node a store holds after inserts, rewrites and deletes, internal and leaf, decodes to the node
/// upstream's decoding of its JSON gives, its unhashed counts included.
#[test]
fn every_stored_node_decodes_as_upstream_decodes_it() -> Result<()> {
    let (dir, store) = fresh()?;
    let mut root =
        store.update(&Root::empty([31; 32]), (0..300u32).map(|n| put(&n.to_be_bytes(), b"first")).collect())?;
    // Each round deletes a fifth of the keys, every one of them present, and rewrites the rest.
    for round in 1..4u32 {
        let change = |n: u32| {
            if n % 5 == round { (n.to_be_bytes().to_vec(), None) } else { put(&n.to_be_bytes(), &round.to_be_bytes()) }
        };
        root = store.update(&root, (0..300u32).map(change).collect())?;
    }
    let conn = raw(&dir)?;
    let mut rows = conn.prepare("SELECT version, path, data FROM nodes")?;
    let rows =
        rows.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?)))?;
    let mut internal = 0;
    for row in rows {
        let (version, path, data) = row?;
        let key = codec::node_key(u64::try_from(version)?, &path)?;
        let upstream: Node = serde_json::from_slice(&data)?;
        internal += usize::from(matches!(upstream, Node::Internal(_)));
        assert_eq!(codec::decode(&key, &data)?, upstream, "{}", String::from_utf8_lossy(&data));
    }
    assert!(internal > 16, "the store holds {internal} internal nodes");
    assert!(root.version.is_some());
    Ok(())
}

/// A child key is the parent's path and then the child's nibble, at the child's version, as packing the
/// path's JSON gave it, at every depth and for every nibble.
#[test]
fn a_child_key_extends_its_parents_path_by_one_nibble() -> Result<()> {
    let mut parent = codec::root_key(9)?;
    assert_eq!(parent, NodeKey::new(9, serde_json::from_slice(br#"{"num_nibbles":0,"bytes":[]}"#)?));
    for depth in 0..64u8 {
        for index in 0..16u8 {
            assert_eq!(
                codec::child_key(&parent, index, 7)?,
                packed(&parent, index, 7)?,
                "depth {depth} nibble {index}"
            );
        }
        parent = codec::child_key(&parent, depth % 16, 9)?;
    }
    assert!(codec::child_key(&parent, 0, 9).is_err(), "a full path has no child");
    assert!(codec::child_key(&codec::root_key(9)?, 16, 9).is_err(), "a nibble is below 16");
    assert!(codec::child_key(&codec::root_key(9)?, 0, 10).is_err(), "a child is no newer than its parent");
    Ok(())
}

/// A node's path key is its nibble count, then its nibbles packed two to a byte, high first, and reads back to
/// the same key at every depth; a count past 64, bytes too few or too many, or a nonzero pad is refused.
#[test]
fn a_path_key_is_its_nibble_count_then_its_packed_nibbles() -> Result<()> {
    let mut key = codec::root_key(3)?;
    assert_eq!(codec::path(&key)?, [0]);
    for depth in 0..64u8 {
        let raw = codec::path(&key)?;
        assert_eq!(raw[0], depth, "the count");
        let mut wire = serde_json::to_value(key.nibble_path())?;
        let upstream: Vec<u8> = serde_json::from_value(wire["bytes"].take())?;
        assert_eq!(raw[1..], upstream, "upstream's packed nibbles at depth {depth}");
        assert_eq!(codec::node_key(3, &raw)?, key, "depth {depth} reads back");
        key = codec::child_key(&key, (depth % 16 * 7 + 3) % 16, 3)?;
    }
    assert_eq!(codec::path(&codec::child_key(&codec::root_key(1)?, 0xa, 1)?)?, [1, 0xa0]);
    let malformed: [&[u8]; 6] = [&[], &[65], &[1], &[1, 0xa0, 0], &[1, 0xab], &[2, 0xab, 0]];
    for malformed in malformed {
        assert!(codec::node_key(1, malformed).is_err(), "{malformed:?}");
    }
    Ok(())
}

// ── Helpers ──

/// The child key packed through the path's JSON, as the codec derived it before.
fn packed(parent: &NodeKey, index: u8, version: u64) -> Result<NodeKey> {
    let mut wire: serde_json::Value = serde_json::to_value(parent.nibble_path())?;
    let nibbles = wire["num_nibbles"].as_u64().expect("a count");
    let bytes = wire["bytes"].as_array_mut().expect("bytes");
    if nibbles.is_multiple_of(2) {
        bytes.push((index << 4).into());
    } else {
        let last = bytes.last_mut().expect("a byte");
        *last = (last.as_u64().expect("a byte") | u64::from(index)).into();
    }
    wire["num_nibbles"] = (nibbles + 1).into();
    Ok(NodeKey::new(version, serde_json::from_value(wire)?))
}

/// Every node a store holds reads back from its compact form as upstream's decoding of its JSON gives it.
#[test]
fn every_stored_node_reads_back_from_its_compact_form() -> Result<()> {
    let (dir, store) = fresh()?;
    let changes = (0..300u32).map(|n| put(&n.to_be_bytes(), &n.to_le_bytes())).collect();
    store.update(&Root::empty([32; 32]), changes)?;
    let conn = raw(&dir)?;
    let mut rows = conn.prepare("SELECT version, path, data FROM nodes")?;
    let rows =
        rows.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?)))?;
    for row in rows {
        let (version, path, data) = row?;
        let key = codec::node_key(u64::try_from(version)?, &path)?;
        let upstream: Node = serde_json::from_slice(&data)?;
        let compact = codec::encode(&upstream, true)?;
        assert!(COMPACT_TAGS.contains(&compact[0]) && compact.len() < data.len(), "{compact:?}");
        assert_eq!(codec::decode(&key, &compact)?, upstream);
    }
    Ok(())
}

/// A compact node is read only when it is exactly one node: an unknown child tag or child type, a byte too
/// few or too many, or a count past its node's is refused.
#[test]
fn a_compact_node_is_refused_unless_it_is_exactly_one_node() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([33; 32]), vec![put(b"a", b"A"), put(b"b", b"B")])?;
    let conn = raw(&dir)?;
    let key = codec::root_key(root.version.expect("a version"))?;
    let data: Vec<u8> =
        conn.query_row("SELECT data FROM nodes WHERE path = ?1 AND version = ?2", (codec::path(&key)?, 0), |row| {
            row.get(0)
        })?;
    let compact = codec::encode(&serde_json::from_slice::<Node>(&data)?, true)?;
    assert_eq!(compact[0], 1, "the root is internal");
    let first_child = 1 + compact[1..].iter().position(|tag| *tag == 1).expect("a child");
    let damaged = [
        [compact.as_slice(), &[0]].concat(),
        compact[..compact.len() - 1].to_vec(),
        [&compact[..first_child], &[2], &compact[first_child + 1..]].concat(),
        [&compact[..first_child + 41], &[2], &compact[first_child + 42..]].concat(),
        [&compact[..compact.len() - 16], &u64::MAX.to_le_bytes(), &compact[compact.len() - 8..]].concat(),
    ];
    for bytes in damaged {
        assert!(codec::decode(&key, &bytes).is_err(), "{bytes:?}");
    }
    Ok(())
}

/// A namespace written compactly stores its new nodes compact and still reads its earlier JSON ones, and
/// another namespace in the same store keeps writing JSON.
#[test]
fn a_compactly_written_namespace_stores_compact_nodes_beside_its_json_ones() -> Result<()> {
    let (dir, store) = fresh()?;
    let (compact, json) = ([34; 32], [35; 32]);
    let first = store.update(&Root::empty(compact), vec![put(b"a", b"A")])?;
    store.write_compactly(compact)?;
    let second = store.update(&first, vec![put(b"b", b"B")])?;
    let other = store.update(&Root::empty(json), vec![put(b"c", b"C")])?;
    let conn = raw(&dir)?;
    let starts = |namespace: [u8; 32], version: u64| -> Result<Vec<u8>> {
        let mut rows = conn.prepare(
            "SELECT data FROM nodes WHERE space = (SELECT id FROM spaces WHERE namespace = ?1) AND version = ?2",
        )?;
        let firsts =
            rows.query_map((&namespace[..], i64::try_from(version)?), |row| Ok(row.get::<_, Vec<u8>>(0)?[0]))?;
        Ok(firsts.collect::<rusqlite::Result<_>>()?)
    };
    assert!(starts(compact, 0)?.iter().all(|byte| !COMPACT_TAGS.contains(byte)), "the first version is JSON");
    let written = starts(compact, 1)?;
    assert!(!written.is_empty() && written.iter().all(|byte| COMPACT_TAGS.contains(byte)), "the second is compact");
    assert!(starts(json, 0)?.iter().all(|byte| !COMPACT_TAGS.contains(byte)), "the other namespace stays JSON");
    assert_eq!((store.read(&second, b"a")?, store.read(&second, b"b")?), (Some(b"A".to_vec()), Some(b"B".to_vec())));
    assert_eq!((store.read(&first, b"a")?, store.read(&other, b"c")?), (Some(b"A".to_vec()), Some(b"C".to_vec())));
    Ok(())
}

/// A compactly written namespace stores each value of at most one chunk whole in its own leaf and a larger
/// one as a descriptor and chunks; every read takes either form, an earlier chunked value included, and a
/// value that moves between the forms leaves no chunk behind.
#[test]
fn a_compactly_written_namespace_stores_values_of_one_chunk_whole() -> Result<()> {
    let (dir, store) = fresh()?;
    let namespace = [36; 32];
    let (small, edge, large) = (vec![7u8; 100], vec![8u8; CHUNK_BYTES], vec![9u8; CHUNK_BYTES + 1]);
    let first = store.update(&Root::empty(namespace), vec![put(b"old", &small), put(b"shrinks", &large)])?;
    store.write_compactly(namespace)?;
    let changes =
        [(&b"small"[..], &small), (b"edge", &edge), (b"large", &large), (b"shrinks", &small), (b"grows", &small)];
    let mut changes: Vec<_> = changes.iter().map(|(key, value)| put(key, value)).collect();
    changes.push(put(b"empty", b""));
    let second = store.update(&first, changes)?;
    let third = store.update(&second, vec![put(b"grows", &large)])?;
    let read = [(&b"old"[..], &small), (b"small", &small), (b"edge", &edge), (b"large", &large), (b"shrinks", &small)];
    for (key, value) in read.into_iter().chain([(&b"grows"[..], &large)]) {
        assert_eq!(store.read(&third, key)?.as_ref(), Some(value), "{}", String::from_utf8_lossy(key));
    }
    assert_eq!(store.read(&third, b"empty")?, Some(Vec::new()));
    let conn = raw(&dir)?;
    let checked = Checked::default();
    let leaf = |root: &Root, key: &[u8]| Points::new(&conn, root, &checked).get(caller_key(&namespace, key)?);
    let chunk = |root: &Root, key: &[u8], index: u64| {
        Points::new(&conn, root, &checked).get(chunk_key(&namespace, caller_key(&namespace, key)?, index)?)
    };
    let first_byte =
        |root: &Root, key: &[u8]| Ok::<_, anyhow::Error>(leaf(root, key)?.and_then(|leaf| leaf.first().copied()));
    assert_eq!(leaf(&third, b"small")?, Some([&[0u8][..], &small].concat()), "a small value is its leaf");
    assert_eq!(leaf(&third, b"edge")?.map(|leaf| leaf.len()), Some(CHUNK_BYTES + 1), "one chunk is its leaf");
    assert_eq!(leaf(&third, b"empty")?, Some(vec![0u8]), "an empty value is its leaf");
    assert_eq!(first_byte(&third, b"old")?, Some(b'{'), "an earlier value keeps its descriptor");
    assert_eq!(first_byte(&third, b"large")?, Some(b'{'), "a value past one chunk has a descriptor");
    assert!(chunk(&third, b"large", 1)?.is_some(), "a value past one chunk has its chunks");
    assert!(chunk(&first, b"shrinks", 1)?.is_some(), "the value had two chunks before it moved");
    for index in 0..2 {
        assert_eq!(chunk(&second, b"shrinks", index)?, None, "a value moved to its leaf left chunk {index}");
    }
    assert_eq!(first_byte(&second, b"grows")?, Some(0), "the value started in its leaf");
    assert!(chunk(&third, b"grows", 1)?.is_some() && first_byte(&third, b"grows")? == Some(b'{'), "then grew past it");
    Ok(())
}
