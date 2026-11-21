//! A proof's reads load only the children upstream's proof can ask for, and every read still proves its value.

use std::cell::Cell;
use std::collections::BTreeMap;

use anyhow::Result;
use jmt::storage::{LeafNode, Node, NodeKey, TreeReader};
use jmt::{JellyfishMerkleTree, KeyHash, RootHash, ValueHash};

use crate::authenticated::reader::proof_children;
use crate::authenticated::tests::{fresh, put};
use crate::authenticated::{Hasher, Root, codec};

/// For every set of two or more children and every nibble, upstream's own proof of a key through that
/// nibble asks only for children `proof_children` names. Every child is a leaf, the case in which
/// upstream reads the most: it reads a sibling alone in its range only when that sibling is a leaf or
/// the range is one wide.
#[test]
fn a_proof_loads_every_child_upstreams_proof_asks_for() -> Result<()> {
    for existing in (1..=u16::MAX).filter(|bits| bits.count_ones() >= 2) {
        let reader = Leaves::new(existing)?;
        for nibble in 0..16u8 {
            let mut key = [0x5a; 32];
            key[0] = nibble << 4 | 0x5;
            reader.asked.set(0);
            let (value, proof) = JellyfishMerkleTree::<_, Hasher>::new(&reader).get_with_proof(KeyHash(key), 0)?;
            proof.verify(RootHash(codec::hash(&reader.root)), KeyHash(key), value.as_deref())?;
            assert_eq!(reader.asked.get() & !proof_children(existing, nibble), 0, "{existing:016b} at {nibble}");
        }
    }
    Ok(())
}

/// Reads of present and absent keys through trees of every depth the fixture reaches give each value,
/// and absence, as they did when every child was loaded.
#[test]
fn reads_through_the_proofs_children_prove_every_value_and_absence() -> Result<()> {
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

// ── Helpers ──

/// A tree at version 0 of one internal root whose children, one at each nibble in its bitmap, are leaves,
/// as upstream reads it, recording the children upstream asks for.
struct Leaves {
    root: Node,
    children: BTreeMap<NodeKey, Node>,
    asked: Cell<u16>,
}

impl Leaves {
    /// The tree with a leaf at each nibble in `existing`.
    /// Error cases:
    /// - a node does not encode or decode
    fn new(existing: u16) -> Result<Self> {
        let (root_key, mut children, mut wire) = (codec::root_key(0)?, BTreeMap::new(), Vec::new());
        for nibble in 0..16u8 {
            if existing & (1 << nibble) == 0 {
                wire.push(serde_json::Value::Null);
                continue;
            }
            let mut key = [0xa5; 32];
            key[0] = nibble << 4 | 0xa;
            let leaf = Node::from(LeafNode::new(KeyHash(key), ValueHash([nibble; 32])));
            wire.push(serde_json::json!({ "hash": codec::hash(&leaf), "version": 0, "node_type": "Leaf" }));
            children.insert(codec::child_key(&root_key, nibble, 0)?, leaf);
        }
        let count = existing.count_ones();
        let root = serde_json::json!({
            "Internal": { "children": { "children": wire, "num_children": count }, "leaf_count": count }
        });
        Ok(Self { root: codec::decode(&root_key, &serde_json::to_vec(&root)?)?, children, asked: Cell::new(0) })
    }
}

impl TreeReader for Leaves {
    /// The root, or a child, recording that upstream asked for it.
    fn get_node_option(&self, key: &NodeKey) -> Result<Option<Node>> {
        if key.nibble_path().num_nibbles() == 0 {
            return Ok(Some(self.root.clone()));
        }
        self.asked.set(self.asked.get() | 1 << u8::from(key.nibble_path().get_nibble(0)));
        Ok(self.children.get(key).cloned())
    }

    /// Any value: the proof's reads, not its value, are under test.
    fn get_value_option(&self, _: u64, _: KeyHash) -> Result<Option<Vec<u8>>> {
        Ok(Some(Vec::new()))
    }

    /// Never asked for by a proof.
    fn get_rightmost_leaf(&self) -> Result<Option<(NodeKey, LeafNode)>> {
        anyhow::bail!("unused")
    }
}
