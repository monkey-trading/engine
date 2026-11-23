//! Point lookups authenticated before upstream traversal, within one SQLite snapshot.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail, ensure};
use jmt::storage::{LeafNode, Node, NodeKey, TreeReader};
use jmt::{KeyHash, ValueHash};
use rusqlite::{Connection, params};

use crate::authenticated::codec;
use crate::authenticated::{EMPTY_HASH, Hasher, MAX_NODE_BYTES, MAX_STORED_VALUE, PHYSICAL_UPDATES, Root};

/// The most nodes an operation keeps checked: a writing reader's node budget.
pub(super) const CHECKED_NODES: usize = node_budget(PHYSICAL_UPDATES);

/// The nodes one operation has checked against an authenticated parent, each with the hash it was
/// checked to have, shared by every reader of the operation: a node is fetched, decoded and hashed once
/// per operation until the operation has checked its limit of nodes and forgotten them, and a reader
/// takes it only where its own checked parent names that hash.
pub(super) struct Checked {
    nodes: RefCell<BTreeMap<NodeKey, (Node, [u8; 32])>>,
    limit: usize,
    reads: RefCell<Reads>,
}

/// The most proved values the operation keeps, and the most bytes of them: a value read once under a root is
/// not proved again under it, until the operation has kept its limit of them and forgotten them.
const READS_KEPT: usize = 4 * PHYSICAL_UPDATES;
const READ_BYTES_KEPT: usize = 256 * 1024;

/// The values the operation has proved under each root, or their proved absence.
#[derive(Default)]
struct Reads {
    values: BTreeMap<(Root, KeyHash), Option<Vec<u8>>>,
    bytes: usize,
}

pub(super) struct Reader<'a> {
    conn: &'a Connection,
    root: Root,
    cache: RefCell<BTreeMap<NodeKey, Node>>,
    /// The parents whose children this reader has loaded and checked.
    prepared: RefCell<BTreeSet<NodeKey>>,
    checked: &'a Checked,
    reads_left: Cell<usize>,
    bytes_left: Cell<usize>,
}

impl Default for Checked {
    /// An operation's checked nodes, at most `CHECKED_NODES` at a time.
    fn default() -> Self {
        Self { nodes: RefCell::default(), limit: CHECKED_NODES, reads: RefCell::default() }
    }
}

#[cfg(test)]
impl Checked {
    /// An operation's checked nodes, at most `limit` at a time.
    pub(super) fn with_limit(limit: usize) -> Self {
        Self { nodes: RefCell::default(), limit, reads: RefCell::default() }
    }

    /// The nodes recorded, for a test to inspect or forge.
    pub(super) fn nodes(&self) -> &RefCell<BTreeMap<NodeKey, (Node, [u8; 32])>> {
        &self.nodes
    }
}

impl Checked {
    /// Records a value proved under `root`, forgetting every recorded one first once it holds its limit.
    fn keep(&self, root: &Root, key: KeyHash, value: &Option<Vec<u8>>) {
        let size = value.as_ref().map_or(0, Vec::len);
        let mut reads = self.reads.borrow_mut();
        if reads.values.len() >= READS_KEPT || reads.bytes.saturating_add(size) > READ_BYTES_KEPT {
            *reads = Reads::default();
        }
        if size <= READ_BYTES_KEPT && reads.values.insert((root.clone(), key), value.clone()).is_none() {
            reads.bytes += size;
        }
    }
}

impl<'a> Reader<'a> {
    /// Creates bounded per-operation state, authenticating the root before it can be traversed, with
    /// the nodes the operation has checked so far.
    /// Error cases:
    /// - the root is absent, malformed, or differs from the caller's trusted hash
    pub(super) fn new(conn: &'a Connection, root: &Root, writing: bool, checked: &'a Checked) -> Result<Self> {
        root.validate()?;
        let keys = if writing { PHYSICAL_UPDATES } else { 1 };
        let nodes = node_budget(keys);
        let reader = Self {
            conn,
            root: root.clone(),
            cache: RefCell::new(BTreeMap::new()),
            prepared: RefCell::new(BTreeSet::new()),
            checked,
            reads_left: Cell::new(nodes + keys * 8),
            bytes_left: Cell::new(nodes * MAX_NODE_BYTES + keys * MAX_STORED_VALUE * 4),
        };
        if let Some(version) = root.version {
            let key = codec::root_key(version)?;
            let node =
                reader.recorded(&key, root.hash).map_or_else(|| reader.check(&key, root.hash, "root node"), Ok)?;
            reader.cache.borrow_mut().insert(key, node);
        }
        Ok(reader)
    }

    /// Returns exact value bytes or proved absence: the walk of the key's path from the root, in which every
    /// node is hashed and matched to its parent's record of it, up to the root's hash, and the value, if the
    /// path ends in its leaf, to the leaf's hash. Upstream's point proof would hash the same nodes and every
    /// sibling beside them to reach the same root.
    /// Error cases:
    /// - bounded storage traversal fails, a node or value does not match its authenticated hash, or a leaf
    ///   stands off its key's path
    pub(super) fn read(&self, key: KeyHash) -> Result<Option<Vec<u8>>> {
        if let Some(value) = self.checked.reads.borrow().values.get(&(self.root.clone(), key)) {
            return Ok(value.clone());
        }
        let value = self.walk(key)?;
        self.checked.keep(&self.root, key, &value);
        Ok(value)
    }

    /// Walks the path of `key` through the authenticated old tree, loading and checking the one child of
    /// each internal node that the path takes.
    /// Error cases:
    /// - as `read`
    fn walk(&self, key: KeyHash) -> Result<Option<Vec<u8>>> {
        let Some(version) = self.root.version else { return Ok(None) };
        let mut node_key = codec::root_key(version)?;
        for depth in 0..=64 {
            let node =
                self.cache.borrow().get(&node_key).cloned().ok_or_else(|| anyhow::anyhow!("missing prepared node"))?;
            match &node {
                Node::Null => {
                    ensure!(depth == 0 && self.root.hash == EMPTY_HASH, "null node under a non-empty root");
                    return Ok(None);
                }
                Node::Leaf(leaf) => {
                    let found = leaf.key_hash();
                    ensure!((0..depth).all(|at| nibble(&found, at) == nibble(&key, at)), "leaf stands off its path");
                    if found != key {
                        return Ok(None);
                    }
                    let value = self
                        .get_value_option(version, key)?
                        .ok_or_else(|| anyhow::anyhow!("missing value row for a leaf"))?;
                    let stored = LeafNode::new(key, ValueHash::with::<Hasher>(&value));
                    ensure!(stored.hash::<Hasher>() == leaf.hash::<Hasher>(), "value does not match its leaf");
                    return Ok(Some(value));
                }
                Node::Internal(internal) => {
                    ensure!(depth < 64, "terminal internal node");
                    let at = nibble(&key, depth);
                    self.prepare_children(&node_key, &node, 1 << at)?;
                    let Some((_, child)) = internal.children_sorted().find(|(n, _)| u8::from(*n) == at) else {
                        return Ok(None);
                    };
                    node_key = codec::child_key(&node_key, at, child.version)?;
                }
            }
        }
        bail!("path deeper than a key")
    }

    /// Fetches the old-tree path of every requested key and, at each internal node on them, the children
    /// `update_children` names for the keys passing through it, before JMT updates them.
    /// Error cases:
    /// - an unavailable/malformed node, child inconsistency, or exhausted finite resource budget
    pub(super) fn prepare(&self, keys: &[KeyHash]) -> Result<()> {
        let Some(version) = self.root.version else { return Ok(()) };
        let mut keys = keys.to_vec();
        keys.sort_unstable();
        keys.dedup();
        self.prepare_update(&codec::root_key(version)?, 0, &keys)
    }

    /// Prepares the subtree at `key`, at `depth`, for an update of the sorted `keys` that pass through it.
    /// Error cases:
    /// - as `prepare`
    fn prepare_update(&self, key: &NodeKey, depth: usize, keys: &[KeyHash]) -> Result<()> {
        let node = self.cache.borrow().get(key).cloned().ok_or_else(|| anyhow::anyhow!("missing prepared node"))?;
        let Node::Internal(internal) = &node else { return Ok(()) };
        ensure!(depth < 64, "terminal internal node");
        let touched = keys.iter().fold(0u16, |bits, query| bits | (1 << nibble(query, depth)));
        self.prepare_children(key, &node, update_children(internal.generate_bitmaps().0, touched))?;
        for group in keys.chunk_by(|a, b| nibble(a, depth) == nibble(b, depth)) {
            let at = nibble(&group[0], depth);
            if let Some((_, child)) = internal.children_sorted().find(|(n, _)| u8::from(*n) == at) {
                self.prepare_update(&codec::child_key(key, at, child.version)?, depth + 1, group)?;
            }
        }
        Ok(())
    }

    /// Looks up the physical value row superseded by an exact update, never as logical authority.
    /// Error cases:
    /// - a bounded point query fails or its returned version is outside the requested range
    pub(super) fn value_version(&self, key: KeyHash) -> Result<Option<u64>> {
        let Some(version) = self.root.version else { return Ok(None) };
        self.charge(0)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT version FROM value_rows WHERE namespace=?1 AND key=?2 AND version<=?3
             ORDER BY version DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![&self.root.namespace[..], &key.0[..], version as i64])?;
        let Some(row) = rows.next()? else { return Ok(None) };
        let found: i64 = row.get(0)?;
        ensure!(found >= 0 && found as u64 <= version, "invalid value row version");
        Ok(Some(found as u64))
    }

    /// Returns the checked native root count for exact per-operation overflow validation.
    /// Error cases:
    /// - the checked root is absent
    pub(super) fn leaf_count(&self) -> Result<usize> {
        let Some(version) = self.root.version else { return Ok(0) };
        let key = codec::root_key(version)?;
        let cache = self.cache.borrow();
        let count = match cache.get(&key) {
            Some(Node::Internal(node)) => node.leaf_count(),
            Some(Node::Leaf(_)) => 1,
            Some(Node::Null) => 0,
            None => bail!("missing checked root"),
        };
        Ok(count)
    }

    /// Reports whether a proposed retirement was actually read from the authenticated old graph.
    pub(super) fn saw(&self, key: &NodeKey) -> bool {
        self.cache.borrow().contains_key(key)
    }

    /// Loads a node with an exact primary-key lookup and validates its fixed-size wire fields.
    /// Error cases:
    /// - budget exhaustion, missing row, oversized BLOB, or malformed node
    fn fetch(&self, key: &NodeKey) -> Result<Node> {
        self.charge(0)?;
        ensure!(key.version() <= self.root.version.unwrap_or(0), "unexpected future node");
        let mut stmt =
            self.conn.prepare_cached("SELECT data FROM nodes WHERE namespace=?1 AND version=?2 AND path=?3")?;
        let mut rows = stmt.query(params![&self.root.namespace[..], key.version() as i64, codec::path(key)?])?;
        let row = rows.next()?.ok_or_else(|| anyhow::anyhow!("missing authenticated node"))?;
        let raw = row.get_ref(0)?.as_blob()?;
        ensure!(raw.len() <= MAX_NODE_BYTES, "oversized node BLOB");
        self.charge_bytes(raw.len())?;
        codec::decode(key, raw)
    }

    /// Materializes the one-hop children in the `wanted` bitmap before JMT can reach its infallible
    /// sibling reader, each checked against its authenticated parent once per reader; a node all of whose
    /// children are loaded is not visited again.
    /// Error cases:
    /// - a direct child is unavailable, malformed, or inconsistent with its authenticated parent
    fn prepare_children(&self, key: &NodeKey, node: &Node, wanted: u16) -> Result<()> {
        let Node::Internal(internal) = node else { return Ok(()) };
        if self.prepared.borrow().contains(key) {
            return Ok(());
        }
        for (nibble, child) in internal.children_sorted() {
            if wanted & (1 << u8::from(nibble)) == 0 {
                continue;
            }
            let child_key = codec::child_key(key, u8::from(nibble), child.version)?;
            if self.cache.borrow().contains_key(&child_key) {
                continue;
            }
            let loaded = match self.recorded(&child_key, child.hash) {
                Some(node) => node,
                None => self.check(&child_key, child.hash, "child")?,
            };
            let count = match &loaded {
                Node::Null => bail!("null child"),
                Node::Leaf(_) => {
                    ensure!(child.is_leaf(), "child type mismatch");
                    1
                }
                Node::Internal(node) => {
                    ensure!(!child.is_leaf(), "child type mismatch");
                    node.leaf_count()
                }
            };
            ensure!(count == child.leaf_count(), "child leaf count mismatch");
            self.cache.borrow_mut().insert(child_key, loaded);
        }
        if internal.generate_bitmaps().0 & !wanted == 0 {
            self.prepared.borrow_mut().insert(key.clone());
        }
        Ok(())
    }

    /// The node at `key` the operation has checked to hash to `hash`, if it has.
    fn recorded(&self, key: &NodeKey, hash: [u8; 32]) -> Option<Node> {
        self.checked.nodes.borrow().get(key).filter(|(_, checked)| *checked == hash).map(|(node, _)| node.clone())
    }

    /// Fetches the node at `key`, checks that it hashes to `hash`, its authenticated parent's for it, and
    /// records it for the operation's other reads, forgetting every recorded node first once the operation
    /// holds its limit of them, so an operation of any size holds no more than one writing reader may.
    /// Error cases:
    /// - the node is unavailable or malformed, or hashes otherwise (a `what` hash mismatch)
    fn check(&self, key: &NodeKey, hash: [u8; 32], what: &str) -> Result<Node> {
        let node = self.fetch(key)?;
        ensure!(codec::hash(&node) == hash, "{what} hash mismatch");
        let mut checked = self.checked.nodes.borrow_mut();
        if checked.len() >= self.checked.limit {
            checked.clear();
        }
        checked.insert(key.clone(), (node.clone(), hash));
        Ok(node)
    }

    /// Charges one indexed SQL operation before it executes.
    /// Error cases:
    /// - the operation or byte budget is exhausted
    fn charge(&self, bytes: usize) -> Result<()> {
        self.reads_left.set(
            self.reads_left
                .get()
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("authenticated read operation budget exhausted"))?,
        );
        self.charge_bytes(bytes)
    }

    /// Charges host bytes before decoding or copying them.
    /// Error cases:
    /// - the byte budget is exhausted
    fn charge_bytes(&self, bytes: usize) -> Result<()> {
        self.bytes_left.set(
            self.bytes_left
                .get()
                .checked_sub(bytes)
                .ok_or_else(|| anyhow::anyhow!("authenticated read byte budget exhausted"))?,
        );
        Ok(())
    }
}

impl TreeReader for Reader<'_> {
    /// Returns only prepared nodes; upstream sibling helpers perform no further disk reads.
    /// Error cases:
    /// - upstream requests a node outside the prepared graph
    fn get_node_option(&self, key: &NodeKey) -> Result<Option<Node>> {
        if self.root.version.is_none() {
            if key == &codec::root_key(u64::MAX)? {
                return Ok(None);
            }
            ensure!(key == &codec::root_key(0)?, "unexpected pre-version node");
            return Ok(Some(Node::Null));
        }
        let node = self
            .cache
            .borrow()
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("node was not reached from a checked parent"))?;
        Ok(Some(node))
    }

    /// Reads a versioned value by primary-key prefix; the caller always verifies its proof.
    /// Error cases:
    /// - invalid version, exhausted budget, non-BLOB value, or oversized value
    fn get_value_option(&self, max_version: u64, key: KeyHash) -> Result<Option<Vec<u8>>> {
        ensure!(self.root.version == Some(max_version), "unexpected value version");
        self.charge(0)?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT version, data FROM value_rows WHERE namespace=?1 AND key=?2 AND version<=?3
             ORDER BY version DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![&self.root.namespace[..], &key.0[..], max_version as i64])?;
        let Some(row) = rows.next()? else { return Ok(None) };
        let version: i64 = row.get(0)?;
        ensure!(version >= 0 && version as u64 <= max_version, "invalid value version");
        let raw = row.get_ref(1)?;
        if raw == rusqlite::types::ValueRef::Null {
            return Ok(None);
        }
        let raw = raw.as_blob()?;
        ensure!(raw.len() <= MAX_STORED_VALUE, "value exceeds storage format");
        self.charge_bytes(raw.len())?;
        Ok(Some(raw.to_vec()))
    }

    /// Refuses restore/iteration: the utility exposes point operations only.
    /// Error cases:
    /// - always; tree restoration is outside this adapter
    fn get_rightmost_leaf(&self) -> Result<Option<(NodeKey, LeafNode)>> {
        bail!("rightmost-leaf scans are unsupported")
    }
}

/// The nodes a reader of `keys` point paths may load: the root and each path's 64 levels of 16 children.
const fn node_budget(keys: usize) -> usize {
    1 + keys * 64 * 16
}

/// The nibble of `key` at `depth`.
fn nibble(key: &KeyHash, depth: usize) -> u8 {
    key.0[depth / 2] >> if depth.is_multiple_of(2) { 4 } else { 0 } & 15
}

/// The nibbles, as a bitmap, of the range of `1 << height` children starting at `start`.
fn range(height: u8, start: u8) -> u16 {
    (((1u32 << (1u32 << height)) - 1) << start) as u16
}

/// The child range holding `nibble` and its sibling range at each of an internal node's four heights.
fn ranges(nibble: u8) -> impl Iterator<Item = u16> {
    (0..4u8).flat_map(move |height| {
        let start = (nibble >> height) << height;
        [range(height, start), range(height, start ^ (1 << height))]
    })
}

/// The children, as a bitmap of nibbles, an update of keys through the `touched` nibbles of an internal
/// node with the `existing` children can read from the old tree: every touched child, and every untouched
/// child alone among the untouched ones in a range holding a touched nibble, or in that range's sibling, at
/// any of the node's four heights. The update reads an old child only where it descends, where it takes a
/// sibling alone in its range, where it descends to the child alone in an absent child's range, and where
/// one child is left to coalesce with, which is alone in its half of the node. A touched child may be
/// rewritten or removed before then, but an untouched one is still there, so a range holding one old child
/// then holds no other untouched child.
pub(super) fn update_children(existing: u16, touched: u16) -> u16 {
    let untouched = existing & !touched;
    let lone = |within: u16| Some(untouched & within).filter(|lone| lone.count_ones() == 1).unwrap_or(0);
    (0..16u8)
        .filter(|nibble| touched & (1 << nibble) != 0)
        .flat_map(ranges)
        .fold(existing & touched, |wanted, within| wanted | lone(within))
}
