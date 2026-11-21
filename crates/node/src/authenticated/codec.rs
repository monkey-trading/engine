//! Validate fixed-size node shapes before calling upstream code with assertions.

use anyhow::{Result, ensure};
use jmt::storage::{LeafNode, NibblePath, Node, NodeKey};
use jmt::{KeyHash, ValueHash};
use serde::{Deserialize, Serialize};

use crate::authenticated::{EMPTY_HASH, Hasher, MAX_NODE_BYTES};

/// `Node::Internal`'s index among upstream's node variants, in its borsh form.
const INTERNAL_VARIANT: u8 = 1;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
enum WireNode {
    Null,
    Leaf(WireLeaf),
    Internal(Box<WireInternal>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireLeaf {
    key_hash: [u8; 32],
    value_hash: [u8; 32],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireInternal {
    children: WireChildren,
    leaf_count: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireChildren {
    children: [Option<WireChild>; 16],
    num_children: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireChild {
    hash: [u8; 32],
    version: u64,
    node_type: WireType,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
enum WireType {
    Leaf,
    Internal { leaf_count: usize },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PathWire {
    num_nibbles: usize,
    bytes: Vec<u8>,
}

/// Serializes a generated node path as its canonical point-lookup key.
/// Error cases:
/// - the upstream key violates the bounded path format
pub(super) fn path(key: &NodeKey) -> Result<Vec<u8>> {
    let raw = serde_json::to_vec(key.nibble_path())?;
    path_wire(&raw)?;
    Ok(raw)
}

/// Creates a root node key: the empty path at `version`.
/// Error cases:
/// - none
pub(super) fn root_key(version: u64) -> Result<NodeKey> {
    Ok(NodeKey::new(version, NibblePath::from_iter(std::iter::empty())))
}

/// Derives a bounded child path: the parent's nibbles and then `index`, collected through upstream's
/// public nibble collection, which packs a path as its own child keys do.
/// Error cases:
/// - the parent is already a full path, or the child index/version is invalid
pub(super) fn child_key(parent: &NodeKey, index: u8, version: u64) -> Result<NodeKey> {
    let path = parent.nibble_path();
    ensure!(path.num_nibbles() < 64 && index < 16 && version <= parent.version(), "invalid child key");
    Ok(NodeKey::new(version, path.nibbles().chain(std::iter::once(index.into())).collect()))
}

/// Decodes bounded, fixed-arity node fields and validates all local invariants first, parsing the stored
/// JSON once.
/// Error cases:
/// - malformed JSON, invalid counters/versions/prefixes, null children, or excessive depth
pub(super) fn decode(key: &NodeKey, raw: &[u8]) -> Result<Node> {
    ensure!(raw.len() <= MAX_NODE_BYTES, "node exceeds storage format");
    let wire: WireNode = serde_json::from_slice(raw)?;
    let path = path_wire(&path(key)?)?;
    match &wire {
        WireNode::Null => ensure!(path.num_nibbles == 0, "non-root null node"),
        WireNode::Leaf(leaf) => {
            for nibble in 0..path.num_nibbles {
                let shift = if nibble % 2 == 0 { 4 } else { 0 };
                ensure!(
                    path.bytes[nibble / 2] >> shift & 15 == leaf.key_hash[nibble / 2] >> shift & 15,
                    "leaf outside its path"
                );
            }
        }
        WireNode::Internal(internal) => {
            ensure!(path.num_nibbles < 64, "internal node at terminal depth");
            let children = &internal.children.children;
            let count = children.iter().flatten().count();
            ensure!(count != 0 && count == internal.children.num_children, "invalid child count");
            let mut total = 0usize;
            for child in children.iter().flatten() {
                ensure!(child.version <= key.version(), "future child version");
                let leaves = match child.node_type {
                    WireType::Leaf => {
                        ensure!(count != 1, "uncoalesced single leaf");
                        1
                    }
                    WireType::Internal { leaf_count } => {
                        ensure!(path.num_nibbles < 63 && leaf_count >= 2, "invalid internal child");
                        leaf_count
                    }
                };
                total = total.checked_add(leaves).ok_or_else(|| anyhow::anyhow!("leaf count overflow"))?;
            }
            ensure!(total == internal.leaf_count, "invalid leaf count");
        }
    }
    node(&wire)
}

/// The upstream node a validated wire node describes, field for field as upstream's own decoding of the
/// same JSON gives it. An internal node has no public constructor, so it is built from its borsh form,
/// which upstream's derive fixes: the variant's index, each of the 16 children as an option of its
/// hash, version and type, the child count, then the leaf count, each count a little-endian `u64`.
/// Error cases:
/// - upstream's decoding refuses the form, which a validated node never gives
fn node(wire: &WireNode) -> Result<Node> {
    let internal = match wire {
        WireNode::Null => return Ok(Node::Null),
        WireNode::Leaf(leaf) => return Ok(LeafNode::new(KeyHash(leaf.key_hash), ValueHash(leaf.value_hash)).into()),
        WireNode::Internal(internal) => internal,
    };
    let mut form = Vec::with_capacity(1 + 16 * 50 + 16);
    form.push(INTERNAL_VARIANT);
    for child in internal.children.children.iter() {
        let Some(child) = child else {
            form.push(0);
            continue;
        };
        form.push(1);
        form.extend_from_slice(&child.hash);
        form.extend_from_slice(&child.version.to_le_bytes());
        match child.node_type {
            WireType::Leaf => form.push(0),
            WireType::Internal { leaf_count } => {
                form.push(1);
                form.extend_from_slice(&u64::try_from(leaf_count)?.to_le_bytes());
            }
        }
    }
    form.extend_from_slice(&u64::try_from(internal.children.num_children)?.to_le_bytes());
    form.extend_from_slice(&u64::try_from(internal.leaf_count)?.to_le_bytes());
    Ok(borsh::from_slice(&form)?)
}

/// Computes node hashes exclusively through upstream implementations.
pub(super) fn hash(node: &Node) -> [u8; 32] {
    match node {
        Node::Null => EMPTY_HASH,
        Node::Leaf(leaf) => leaf.hash::<Hasher>(),
        Node::Internal(internal) => internal.hash::<Hasher>(),
    }
}

/// Validates the generated serde path without trusting its stored length.
/// Error cases:
/// - excessive length, inconsistent byte count, or nonzero padding
fn path_wire(raw: &[u8]) -> Result<PathWire> {
    ensure!(raw.len() <= 256, "oversized path");
    let path: PathWire = serde_json::from_slice(raw)?;
    ensure!(path.num_nibbles <= 64 && path.bytes.len() == path.num_nibbles.div_ceil(2), "invalid path length");
    if !path.num_nibbles.is_multiple_of(2) {
        ensure!(path.bytes.last().is_some_and(|b| b & 15 == 0), "invalid path padding");
    }
    Ok(path)
}
