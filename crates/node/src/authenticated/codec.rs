//! Validate fixed-size node shapes before calling upstream code with assertions.

use anyhow::{Result, bail, ensure};
use jmt::storage::{LeafNode, NibblePath, Node, NodeKey};
use jmt::{KeyHash, ValueHash};
use serde::Deserialize;

use crate::authenticated::{EMPTY_HASH, Hasher, MAX_NODE_BYTES};

/// `Node::Null`'s, `Node::Internal`'s and `Node::Leaf`'s indexes among upstream's node variants, in its
/// borsh form, which a stored node starts with when it is compact. JSON starts with none of them.
const NULL_VARIANT: u8 = 0;
const INTERNAL_VARIANT: u8 = 1;
const LEAF_VARIANT: u8 = 2;

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

/// A node path's nibble count and the bytes that pack its nibbles, two to a byte, high nibble first.
struct PathWire {
    num_nibbles: usize,
    bytes: Vec<u8>,
}

/// Serializes a generated node path as its canonical point-lookup key: its nibble count in one byte, then the
/// bytes that pack its nibbles, two to a byte, high nibble first, an odd path's last low nibble zero.
/// Error cases:
/// - the upstream key violates the bounded path format
pub(super) fn path(key: &NodeKey) -> Result<Vec<u8>> {
    let nibbles = key.nibble_path().num_nibbles();
    ensure!(nibbles <= 64, "invalid path length");
    let mut raw = Vec::with_capacity(1 + nibbles.div_ceil(2));
    raw.push(u8::try_from(nibbles)?);
    for (at, nibble) in key.nibble_path().nibbles().enumerate() {
        let nibble = u8::from(nibble);
        if at.is_multiple_of(2) {
            raw.push(nibble << 4);
        } else {
            *raw.last_mut().expect("an even nibble pushed its byte") |= nibble;
        }
    }
    path_wire(&raw)?;
    Ok(raw)
}

/// The key of the node stored at `version` under the path key `raw` (`path`).
/// Error cases:
/// - the path key is malformed
pub(super) fn node_key(version: u64, raw: &[u8]) -> Result<NodeKey> {
    let path = path_wire(raw)?;
    let nibbles =
        (0..path.num_nibbles).map(|at| (path.bytes[at / 2] >> if at.is_multiple_of(2) { 4 } else { 0 } & 15).into());
    Ok(NodeKey::new(version, nibbles.collect()))
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

/// A node as it is stored: upstream's compact borsh form, or its JSON.
/// Error cases:
/// - the node does not encode
pub(super) fn encode(node: &Node, compact: bool) -> Result<Vec<u8>> {
    Ok(if compact { borsh::to_vec(node)? } else { serde_json::to_vec(node)? })
}

/// Decodes bounded, fixed-arity node fields, stored compact or as JSON, and validates all local
/// invariants first, parsing the stored bytes once.
/// Error cases:
/// - malformed bytes, invalid counters/versions/prefixes, null children, or excessive depth
pub(super) fn decode(key: &NodeKey, raw: &[u8]) -> Result<Node> {
    ensure!(raw.len() <= MAX_NODE_BYTES, "node exceeds storage format");
    let wire = match raw.first() {
        Some(&(NULL_VARIANT | INTERNAL_VARIANT | LEAF_VARIANT)) => compact(raw)?,
        _ => serde_json::from_slice(raw)?,
    };
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

/// The wire form of upstream's borsh form of a node, read strictly: known tags only, every count in range,
/// and no byte past the node.
/// Error cases:
/// - an unknown tag, a short or long input, or a count past `usize`
fn compact(raw: &[u8]) -> Result<WireNode> {
    let mut bytes = Bytes(raw);
    let wire = match bytes.byte()? {
        NULL_VARIANT => WireNode::Null,
        LEAF_VARIANT => WireNode::Leaf(WireLeaf { key_hash: bytes.hash()?, value_hash: bytes.hash()? }),
        INTERNAL_VARIANT => {
            let mut children: [Option<WireChild>; 16] = Default::default();
            for child in &mut children {
                *child = match bytes.byte()? {
                    0 => None,
                    1 => Some(WireChild {
                        hash: bytes.hash()?,
                        version: bytes.u64()?,
                        node_type: match bytes.byte()? {
                            0 => WireType::Leaf,
                            1 => WireType::Internal { leaf_count: usize::try_from(bytes.u64()?)? },
                            _ => bail!("invalid compact child type"),
                        },
                    }),
                    _ => bail!("invalid compact child tag"),
                };
            }
            let num_children = usize::try_from(bytes.u64()?)?;
            let leaf_count = usize::try_from(bytes.u64()?)?;
            WireNode::Internal(Box::new(WireInternal { children: WireChildren { children, num_children }, leaf_count }))
        }
        _ => bail!("invalid compact node tag"),
    };
    ensure!(bytes.0.is_empty(), "bytes past a compact node");
    Ok(wire)
}

/// A compact node's bytes, read in order.
struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    /// The next `n` bytes.
    /// Error cases:
    /// - fewer are left
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.0.len() >= n, "short compact node");
        let (taken, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(taken)
    }

    /// The next byte.
    /// Error cases:
    /// - none is left
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// The next little-endian `u64`.
    /// Error cases:
    /// - fewer than eight bytes are left
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }

    /// The next 32-byte hash.
    /// Error cases:
    /// - fewer than 32 bytes are left
    fn hash(&mut self) -> Result<[u8; 32]> {
        Ok(self.take(32)?.try_into()?)
    }
}

/// The key hash of a stored leaf node, compact or JSON, or `None` for another node.
/// Error cases:
/// - the stored bytes are no node
pub(super) fn leaf_key(raw: &[u8]) -> Result<Option<[u8; 32]>> {
    ensure!(raw.len() <= MAX_NODE_BYTES, "node exceeds storage format");
    let wire = match raw.first() {
        Some(&(NULL_VARIANT | INTERNAL_VARIANT | LEAF_VARIANT)) => compact(raw)?,
        _ => serde_json::from_slice(raw)?,
    };
    Ok(match wire {
        WireNode::Leaf(leaf) => Some(leaf.key_hash),
        WireNode::Null | WireNode::Internal(_) => None,
    })
}

/// Computes node hashes exclusively through upstream implementations.
pub(super) fn hash(node: &Node) -> [u8; 32] {
    match node {
        Node::Null => EMPTY_HASH,
        Node::Leaf(leaf) => leaf.hash::<Hasher>(),
        Node::Internal(internal) => internal.hash::<Hasher>(),
    }
}

/// Validates a path key (`path`) without trusting its stored length.
/// Error cases:
/// - excessive length, inconsistent byte count, or nonzero padding
fn path_wire(raw: &[u8]) -> Result<PathWire> {
    ensure!(!raw.is_empty() && raw.len() <= 33, "invalid path key length");
    let path = PathWire { num_nibbles: usize::from(raw[0]), bytes: raw[1..].to_vec() };
    ensure!(path.num_nibbles <= 64 && path.bytes.len() == path.num_nibbles.div_ceil(2), "invalid path length");
    if !path.num_nibbles.is_multiple_of(2) {
        ensure!(path.bytes.last().is_some_and(|b| b & 15 == 0), "invalid path padding");
    }
    Ok(path)
}
