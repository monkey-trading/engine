//! A read walks and checks only the nodes on its key's path, and still proves every value and absence.

use anyhow::Result;
use jmt::storage::{LeafNode, Node};
use jmt::{KeyHash, ValueHash};
use rusqlite::params;

use crate::authenticated::reader::{Checked, Reader};
use crate::authenticated::tests::{fresh, put, raw};
use crate::authenticated::{Hasher, Root, codec, key_hash};

/// Reads of present and absent keys through trees of every depth the fixture reaches give each value, and
/// absence.
#[test]
fn reads_prove_every_value_and_absence() -> Result<()> {
    let (_dir, store) = fresh()?;
    let mut root = Root::empty([41; 32]);
    for round in 0..3u32 {
        let changes = (round * 400..(round + 1) * 400).map(|n| put(&n.to_be_bytes(), &n.to_le_bytes())).collect();
        root = store.update(&root, changes)?;
    }
    for n in 0..1_400u32 {
        let expected = (n < 1_200).then(|| n.to_le_bytes().to_vec());
        assert_eq!(store.read(&root, &n.to_be_bytes())?, expected, "key {n}");
    }
    Ok(())
}

/// A read of one key checks one node a level of its path, with none of the siblings beside it, and the
/// same read again, of a value or of an absence, checks nothing more.
#[test]
fn a_read_checks_only_the_nodes_on_its_path() -> Result<()> {
    let (dir, store) = fresh()?;
    let namespace = [42; 32];
    let changes = (0..1_200u32).map(|n| put(&n.to_be_bytes(), &n.to_le_bytes())).collect();
    let root = store.update(&Root::empty(namespace), changes)?;
    let conn = raw(&dir)?;
    let checked = Checked::default();
    let reader = Reader::new(&conn, &root, true, &checked)?;
    let present = key_hash(&namespace, &7u32.to_be_bytes())?;
    assert!(reader.read(present)?.is_some());
    let path = checked.nodes().borrow().len();
    assert!((2..=6).contains(&path), "{path} nodes checked for one key in 1,200");
    assert!(reader.read(present)?.is_some());
    assert_eq!(checked.nodes().borrow().len(), path, "a second read of the key checks nothing more");
    assert_eq!(reader.read(key_hash(&namespace, b"absent")?)?, None);
    Ok(())
}

/// A leaf that stands where its key's path does not lead is refused, though every hash on the way matches:
/// a leaf at nibble 3 of the root holds a key whose first nibble is 5, beside a leaf in its place at nibble 9,
/// and a read of a key of nibble 3 finds it there.
#[test]
fn a_leaf_off_its_keys_path_is_refused() -> Result<()> {
    let (dir, _store) = fresh()?;
    let namespace = [43; 32];
    let root_key = codec::root_key(0)?;
    let conn = raw(&dir)?;
    let mut wire = vec![serde_json::Value::Null; 16];
    let mut held = [0xa5; 32];
    // The leaf at each slot holds the key whose first nibble is its own, but at slot 3 it is 5's.
    for (slot, first) in [(3u8, 0x5a), (9, 0x9a)] {
        held[0] = first;
        let value = vec![slot];
        let leaf = Node::from(LeafNode::new(KeyHash(held), ValueHash::with::<Hasher>(&value)));
        wire[usize::from(slot)] = serde_json::json!({ "hash": codec::hash(&leaf), "version": 0, "node_type": "Leaf" });
        conn.execute(
            "INSERT INTO nodes(namespace,version,path,data) VALUES(?1,0,?2,?3)",
            params![&namespace[..], codec::path(&codec::child_key(&root_key, slot, 0)?)?, codec::encode(&leaf, false)?],
        )?;
        conn.execute(
            "INSERT INTO value_rows(namespace,key,version,data) VALUES(?1,?2,0,?3)",
            params![&namespace[..], &held[..], value],
        )?;
    }
    let internal = serde_json::json!({
        "Internal": { "children": { "children": wire, "num_children": 2 }, "leaf_count": 2 }
    });
    let top = codec::decode(&root_key, &serde_json::to_vec(&internal)?)?;
    conn.execute(
        "INSERT INTO nodes(namespace,version,path,data) VALUES(?1,0,?2,?3)",
        params![&namespace[..], codec::path(&root_key)?, codec::encode(&top, false)?],
    )?;
    let root = Root { namespace, version: Some(0), hash: codec::hash(&top) };
    let checked = Checked::default();
    let reader = Reader::new(&conn, &root, true, &checked)?;
    let mut asked = held;
    asked[0] = 0x3a;
    assert!(reader.read(KeyHash(asked)).is_err(), "a leaf off the path proved an absence");
    asked[0] = 0x9a;
    assert_eq!(reader.read(KeyHash(asked))?, Some(vec![9]), "a leaf on its path reads");
    Ok(())
}
