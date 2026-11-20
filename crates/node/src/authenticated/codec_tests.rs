//! The node codec gives what upstream's own decoding of the stored JSON gives, parsing it once.

use anyhow::Result;
use jmt::storage::{NibblePath, Node, NodeKey};

use crate::authenticated::tests::{fresh, put, raw};
use crate::authenticated::{Root, codec};

/// Every node a store holds after inserts, rewrites and deletes, internal nodes of each child count and
/// leaves at each depth among them, decodes to the node upstream's decoding of its JSON gives, its
/// unhashed counts included.
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
        let key = NodeKey::new(u64::try_from(version)?, serde_json::from_slice::<NibblePath>(&path)?);
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
